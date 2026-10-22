// How a node gathers every bundle the blobs program records, serves blobs to members over
// halo and receipts what it holds. noded starts it, tells it each applied block and asks it
// for a blob it lacks, from the members and then from a source it names.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use abi::{BlobId, Refusal, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::ed25519::PublicKey;
use halo::{Class, Halo, Inbound, Links, Paced, Streams};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::contract::{Bundle, Entry, bundle_of};
use crate::{Blobs, Collision, Error, names, size_of};

const BACKLOG: NonZeroUsize = NonZeroUsize::new(64).expect("nonzero");
const SERVING: NonZeroUsize = NonZeroUsize::new(64).expect("nonzero");
const GATHERERS: NonZeroUsize = NonZeroUsize::new(4).expect("nonzero");
const PAUSE: Duration = Duration::from_secs(1);
const PAUSE_MOST: Duration = Duration::from_secs(60);
const CHUNK: usize = 64 << 10;
/// This node's own lane table does not serve the blobs lane, so it asks no member.
const NO_LANE: &str = "no_lane";
const NO_MEMBER_GAVE_IT: &str = "no_member_gave_it";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub backlog: NonZeroUsize,
    pub serving: NonZeroUsize,
    pub gatherers: NonZeroUsize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            backlog: BACKLOG,
            serving: SERVING,
            gatherers: GATHERERS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub bundle: u64,
    pub posted: Option<Vec<u8>>,
}

#[derive(Debug, BorshSerialize, BorshDeserialize)]
struct Ask {
    id: BlobId,
    offset: u64,
}

#[derive(Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
enum Answer {
    Have { len: u64 },
    Lacks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Local {
    Matches,
    Mismatched,
    Absent,
}

struct Open {
    bundle: Bundle,
    posted: Option<Vec<u8>>,
}

struct SentBy {
    key: PublicKey,
    carried_until: u64,
}

struct Wanted {
    id: BlobId,
    declared: Option<Entry>,
    due: Instant,
    rounds: u32,
    pulling: bool,
}

enum Next {
    Pull(BlobId, Option<Entry>),
    Until(Instant),
    Idle,
}

#[derive(Default)]
struct State {
    open: BTreeMap<u64, Open>,
    wanted: BTreeMap<BlobId, Wanted>,
    senders: HashMap<BlobId, SentBy>,
}

struct Owner {
    generation: u64,
    class: Class,
    ended: watch::Sender<()>,
}

#[derive(Default)]
struct Pulls {
    owners: HashMap<BlobId, Owner>,
    live: HashSet<u64>,
    generations: u64,
}

struct Owning {
    inner: Arc<Inner>,
    id: BlobId,
    generation: u64,
    superseded: watch::Receiver<()>,
}

impl Drop for Owning {
    fn drop(&mut self) {
        let mut pulls = self.inner.pulls();
        pulls.live.remove(&self.generation);
        let mine = pulls
            .owners
            .get(&self.id)
            .is_some_and(|owner| owner.generation == self.generation);
        if mine {
            pulls.owners.remove(&self.id);
        }
    }
}

enum Turn {
    Mine(Owning),
    Joined(watch::Receiver<()>),
}

enum Pulled {
    Landed,
    Superseded,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Kept {
    Landed,
    Elsewhere,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Fetched {
    Landed,
    Unheld,
}

pub trait Source {
    fn blob(
        &self,
        id: BlobId,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, &'static str>> + Send;
}

enum Nowhere {}

impl Source for Nowhere {
    async fn blob(&self, _: BlobId) -> Result<Option<Vec<u8>>, &'static str> {
        match *self {}
    }
}

struct Inner {
    halo: Arc<Halo>,
    lane: Streams<Arc<Links>>,
    store: Blobs,
    state: Mutex<State>,
    work: Notify,
    pulls: Mutex<Pulls>,
    uploads: AtomicU64,
    staged_uploads: Mutex<HashMap<BlobId, BTreeMap<u64, u64>>>,
    applied: watch::Sender<u64>,
    landed: watch::Sender<u64>,
}

pub struct Service {
    inner: Arc<Inner>,
    tasks: Vec<AbortHandle>,
}

impl Drop for Service {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Service {
    pub fn start(
        halo: Arc<Halo>,
        store: Blobs,
        bundles: impl IntoIterator<Item = (u64, Bundle)>,
        config: Config,
    ) -> crate::Result<Service> {
        store.clear_staging()?;
        let generations = leftovers(&store, &store.partials()?)?;
        let lane = halo
            .plane()
            .streams(crate::contract::LANE, config.backlog.get())
            .map_err(|taken| std::io::Error::new(std::io::ErrorKind::AddrInUse, taken))?;
        let inner = Arc::new(Inner {
            halo,
            lane,
            store,
            state: Mutex::default(),
            work: Notify::new(),
            pulls: Mutex::new(Pulls {
                generations,
                ..Pulls::default()
            }),
            uploads: AtomicU64::new(0),
            staged_uploads: Mutex::default(),
            applied: watch::Sender::new(0),
            landed: watch::Sender::new(0),
        });
        for (n, bundle) in bundles {
            inner.record(n, bundle);
        }
        let mut tasks = vec![tokio::spawn(serving(inner.clone(), config.serving)).abort_handle()];
        for _ in 0..config.gatherers.get() {
            tasks.push(tokio::spawn(gathering(inner.clone())).abort_handle());
        }
        Ok(Service { inner, tasks })
    }

    pub fn applied(&self, height: u64, writes: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Vec<Due> {
        for (key, value) in writes {
            let Some(n) = bundle_of(key) else {
                continue;
            };
            let Some(bundle) = value.as_deref().and_then(|bytes| abi::decode(bytes).ok()) else {
                continue;
            };
            self.inner.record(n, bundle);
        }
        self.inner.unsent(height);
        self.inner.unstage_unlandable(height);
        self.inner.applied.send_replace(height);
        self.inner.due()
    }

    pub fn sent(
        &self,
        ids: impl IntoIterator<Item = BlobId>,
        from: &PublicKey,
        carried_until: u64,
    ) {
        let mut state = self.inner.state();
        for id in ids {
            let sender = SentBy {
                key: from.clone(),
                carried_until,
            };
            state.senders.insert(id, sender);
        }
    }

    pub fn posted(&self, bundle: u64, receipt: Vec<u8>) {
        self.inner.posted(bundle, receipt);
    }

    pub fn posts(&self) -> Posts {
        Posts {
            inner: self.inner.clone(),
        }
    }

    pub async fn fetch(&self, id: BlobId, class: Class) -> Fetched {
        self.inner.fetch(id, class, None::<&Nowhere>).await
    }

    pub async fn fetch_or(&self, id: BlobId, class: Class, source: &impl Source) -> Fetched {
        self.inner.fetch(id, class, Some(source)).await
    }

    pub async fn seek(&self, id: BlobId) -> bool {
        self.inner.seek(id).await
    }

    pub async fn keep(&self, id: BlobId, framed: Vec<u8>) -> Result<Kept, &'static str> {
        self.inner.keep(id, framed, Class::Client).await
    }

    pub async fn stage(
        &self,
        entries: &[Entry],
        body: &mut (impl AsyncRead + Unpin),
        last_landing: u64,
    ) -> Result<Staged, Refusal> {
        let mut staging = Staging(Staged {
            inner: self.inner.clone(),
            uploads: Vec::new(),
        });
        let staged = {
            let staging_all =
                self.inner
                    .stage_all(entries, body, last_landing, &mut staging.0.uploads);
            tokio::select! {
                biased;
                () = landing_passed(&self.inner.applied, last_landing) => Err(Refusal::new(
                    reason::ANCHOR,
                    "the upload's put can no longer land",
                )),
                staged = staging_all => staged,
            }
        };
        staged?;
        Ok(Staged {
            inner: self.inner.clone(),
            uploads: std::mem::take(&mut staging.0.uploads),
        })
    }

    pub fn store(&self) -> &Blobs {
        &self.inner.store
    }

    pub fn landed(&self) -> watch::Receiver<u64> {
        self.inner.landed.subscribe()
    }
}

pub struct Staged {
    inner: Arc<Inner>,
    uploads: Vec<(BlobId, u64)>,
}

impl Staged {
    pub fn refused(self) {
        self.unclaim();
    }

    fn unclaim(&self) {
        for (id, upload) in &self.uploads {
            self.inner.unclaim(id, *upload);
        }
    }
}

struct Staging(Staged);

async fn landing_passed(applied: &watch::Sender<u64>, last_landing: u64) {
    let mut applied = applied.subscribe();
    let _ = applied.wait_for(|height| *height >= last_landing).await;
}

impl Drop for Staging {
    fn drop(&mut self) {
        self.0.unclaim();
    }
}

struct Temp {
    path: PathBuf,
}

impl Temp {
    fn create(path: PathBuf) -> std::io::Result<(Temp, tokio::fs::File)> {
        let temp = Temp { path };
        let file = std::fs::File::create(&temp.path)?;
        Ok((temp, tokio::fs::File::from_std(file)))
    }

    fn rename(self, to: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, to)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Clone)]
pub struct Posts {
    inner: Arc<Inner>,
}

impl Posts {
    pub fn posted(&self, bundle: u64, receipt: Vec<u8>) {
        self.inner.posted(bundle, receipt);
    }
}

fn unread(error: std::io::Error) -> Refusal {
    Refusal::new(
        reason::INVALID_INPUT,
        format!("the upload could not be read: {error}"),
    )
}

fn unstored(error: impl std::fmt::Display) -> Refusal {
    Refusal::new(
        reason::NOT_FOUND,
        format!("the node could not stage the upload: {error}"),
    )
}

fn pause(rounds: u32) -> Duration {
    PAUSE.saturating_mul(rounds).min(PAUSE_MOST)
}

async fn closed(pulling: &mut watch::Receiver<()>) {
    while pulling.changed().await.is_ok() {}
}

async fn unless_superseded<T>(
    superseded: &mut watch::Receiver<()>,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = closed(superseded) => None,
        done = work => Some(done),
    }
}

fn leftovers(store: &Blobs, partials: &Path) -> std::io::Result<u64> {
    let mut highest = 0;
    for entry in std::fs::read_dir(partials)? {
        let entry = entry?;
        let file = entry.file_name();
        let Some(name) = file.to_str() else {
            continue;
        };
        let numbered = name
            .rsplit_once('.')
            .and_then(|(stem, n)| Some((stem, n.parse::<u64>().ok()?)));
        let stem = match numbered {
            Some((stem, generation)) => {
                highest = highest.max(generation);
                stem
            }
            None => name,
        };
        if store.holds_named(stem) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(highest)
}

fn generation_path(partial: &Path, generation: u64) -> PathBuf {
    let mut name = partial.as_os_str().to_owned();
    name.push(format!(".{generation}"));
    PathBuf::from(name)
}

fn seed(partial: &Path, generation: u64, live: &HashSet<u64>) -> std::io::Result<PathBuf> {
    let mine = generation_path(partial, generation);
    let named = partial.file_name().and_then(|name| name.to_str());
    let (Some(dir), Some(name)) = (partial.parent(), named) else {
        return Ok(mine);
    };
    let mut older = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file = entry.file_name();
        let Some(rest) = file.to_str().and_then(|file| file.strip_prefix(name)) else {
            continue;
        };
        let owner = match rest {
            "" => None,
            rest => match rest.strip_prefix('.').and_then(|n| n.parse::<u64>().ok()) {
                Some(owner) if owner < generation => Some(owner),
                _ => continue,
            },
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        older.push((meta.len(), owner, entry.path()));
    }
    older.sort_by_key(|(len, ..)| std::cmp::Reverse(*len));
    let written = |owner: Option<u64>| owner.is_some_and(|owner| live.contains(&owner));
    let mut seeded = false;
    for (_, owner, path) in older {
        if !seeded {
            let taken = match written(owner) {
                true => std::fs::copy(&path, &mine).map(drop),
                false => std::fs::rename(&path, &mine),
            };
            match taken {
                Ok(()) => {
                    seeded = true;
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
        }
        if !written(owner) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(mine)
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("the blob state lock is never poisoned")
    }

    fn posted(&self, bundle: u64, receipt: Vec<u8>) {
        if let Some(open) = self.state().open.get_mut(&bundle) {
            open.posted = Some(receipt);
        }
    }

    fn local(&self, entry: &Entry) -> Local {
        let staged_unadopted = !self.adopt_staged(&entry.id);
        if staged_unadopted {
            return Local::Absent;
        }
        let Ok(Some(header)) = self.store.stat(&entry.id) else {
            return Local::Absent;
        };
        let size = self.store.size(&entry.id).ok().flatten();
        let sized = size == Some(entry.len);
        let kinded = header.kind == entry.kind;
        if sized && kinded {
            return Local::Matches;
        }
        Local::Mismatched
    }

    fn adopt_staged(&self, id: &BlobId) -> bool {
        let mut staged_uploads = self.staged_uploads();
        let staged = self.store.staged(id);
        if !staged.is_file() {
            return true;
        }
        let held = self.store.has(id) || self.store.adopt(id, &staged).is_ok();
        if !held {
            return false;
        }
        staged_uploads.remove(id);
        self.unstage(id);
        true
    }

    fn staged_uploads(&self) -> MutexGuard<'_, HashMap<BlobId, BTreeMap<u64, u64>>> {
        self.staged_uploads
            .lock()
            .expect("the staged uploads lock is never poisoned")
    }

    fn claim(&self, id: BlobId, upload: u64, last_landing: u64, temp: Temp) -> std::io::Result<()> {
        let mut staged_uploads = self.staged_uploads();
        temp.rename(&self.store.staged(&id))?;
        staged_uploads
            .entry(id)
            .or_default()
            .insert(upload, last_landing);
        Ok(())
    }

    fn unclaim(&self, id: &BlobId, upload: u64) {
        let mut staged_uploads = self.staged_uploads();
        let Some(uploads) = staged_uploads.get_mut(id) else {
            return;
        };
        uploads.remove(&upload);
        if uploads.is_empty() {
            staged_uploads.remove(id);
            self.unstage(id);
        }
    }

    fn unstage_unlandable(&self, height: u64) {
        let mut staged_uploads = self.staged_uploads();
        let mut unlandable = Vec::new();
        staged_uploads.retain(|id, uploads| {
            uploads.retain(|_, last_landing| *last_landing > height);
            let still_landable = !uploads.is_empty();
            if !still_landable {
                unlandable.push(*id);
            }
            still_landable
        });
        for id in unlandable {
            self.unstage(&id);
        }
    }

    fn unstage(&self, id: &BlobId) {
        let removed = std::fs::remove_file(self.store.staged(id));
        let Err(error) = removed else {
            return;
        };
        if error.kind() == std::io::ErrorKind::NotFound {
            return;
        }
        tracing::warn!(target: "ducktape::blob", event = "blob_unstaged", ?id, %error, reason = "staging_unremoved", "a staged upload no put waits for any more could not be removed");
    }

    fn record(&self, n: u64, bundle: Bundle) {
        let absent: Vec<Entry> = bundle
            .blobs
            .iter()
            .filter(|entry| self.local(entry) == Local::Absent)
            .cloned()
            .collect();
        let mut state = self.state();
        let now = Instant::now();
        for entry in absent {
            let want = state.wanted.entry(entry.id).or_insert(Wanted {
                id: entry.id,
                declared: None,
                due: now,
                rounds: 0,
                pulling: false,
            });
            want.declared = Some(entry);
        }
        let posted = state.open.remove(&n).and_then(|open| open.posted);
        state.open.insert(n, Open { bundle, posted });
        drop(state);
        self.work.notify_waiters();
    }

    fn unsent(&self, height: u64) {
        let mut state = self.state();
        let State {
            wanted, senders, ..
        } = &mut *state;
        senders.retain(|id, sender| wanted.contains_key(id) || height < sender.carried_until);
    }

    fn due(&self) -> Vec<Due> {
        let me = self.halo.me().clone();
        let validator = self.halo.members().validator(&me);
        let open: Vec<(u64, Vec<Entry>)> = self
            .state()
            .open
            .iter()
            .map(|(n, open)| (*n, open.bundle.blobs.clone()))
            .collect();
        let complete: Vec<(u64, bool)> = open
            .into_iter()
            .map(|(n, blobs)| {
                let all = blobs
                    .iter()
                    .all(|entry| self.local(entry) == Local::Matches);
                (n, all)
            })
            .collect();
        let mut state = self.state();
        let mut due = Vec::new();
        for (n, all) in complete {
            let Some(open) = state.open.get_mut(&n) else {
                continue;
            };
            let held = open.bundle.held();
            let receipted = open.bundle.receipted_by(me.as_ref());
            let settled = all && (held || receipted);
            if settled {
                state.open.remove(&n);
                continue;
            }
            let receipts = validator && all && !held && !receipted;
            if receipts {
                due.push(Due {
                    bundle: n,
                    posted: open.posted.clone(),
                });
            }
        }
        due
    }

    fn next(&self) -> Next {
        let now = Instant::now();
        let mut state = self.state();
        let mut earliest: Option<Instant> = None;
        for want in state.wanted.values_mut() {
            if want.pulling {
                continue;
            }
            if want.due <= now {
                want.pulling = true;
                return Next::Pull(want.id, want.declared.clone());
            }
            earliest = Some(earliest.map_or(want.due, |at| at.min(want.due)));
        }
        match earliest {
            Some(at) => Next::Until(at),
            None => Next::Idle,
        }
    }

    fn gathered(&self, id: &BlobId, round: Result<(), &'static str>) {
        let mut state = self.state();
        let Err(missed) = round else {
            state.wanted.remove(id);
            state.senders.remove(id);
            return;
        };
        let Some(want) = state.wanted.get_mut(id) else {
            return;
        };
        want.pulling = false;
        want.rounds += 1;
        want.due = Instant::now() + pause(want.rounds);
        let rounds = want.rounds;
        let worth_a_line = rounds == 1 || rounds.is_multiple_of(10);
        if worth_a_line {
            tracing::warn!(target: "ducktape::blob", event = "blob_ungathered", ?id, attempts = rounds, reason = missed, "no member gave a blob this node wants; the node asks again");
        }
    }

    fn sender(&self, id: &BlobId) -> Option<PublicKey> {
        Some(self.state().senders.get(id)?.key.clone())
    }

    fn declared(&self, id: &BlobId) -> Option<Entry> {
        let state = self.state();
        state.open.values().find_map(|open| {
            open.bundle
                .blobs
                .iter()
                .find(|entry| entry.id == *id)
                .cloned()
        })
    }

    /// Asks each candidate member in turn until one gives the blob; failing that, the reason
    /// the round names.
    async fn round(
        self: &Arc<Self>,
        id: &BlobId,
        declared: Option<&Entry>,
        class: Class,
    ) -> Result<(), &'static str> {
        // The lane is this node's own table: unserved, no member is asked and the round names
        // the table, not the members.
        if !self.lane.served() {
            return Err(NO_LANE);
        }
        let me = self.halo.me();
        let sender = self.sender(id);
        for peer in self.halo.members().candidates(me, sender.as_ref()) {
            let pulled = self.pull(&peer, id, declared, class).await;
            match pulled {
                Ok(()) => return Ok(()),
                Err(reason) => {
                    tracing::debug!(target: "ducktape::blob", ?id, %peer, reason, "a member did not give a blob");
                }
            }
        }
        Err(NO_MEMBER_GAVE_IT)
    }

    async fn fetch(
        self: &Arc<Self>,
        id: BlobId,
        class: Class,
        source: Option<&impl Source>,
    ) -> Fetched {
        let declared = self.declared(&id);
        let mut rounds = 0u32;
        loop {
            if self.store.has(&id) {
                return Fetched::Landed;
            }
            let Err(missed) = self.round(&id, declared.as_ref(), class).await else {
                return Fetched::Landed;
            };
            rounds += 1;
            let worth_a_line = rounds == 1 || rounds.is_multiple_of(10);
            if let Some(source) = source {
                let every_member_asked = missed == NO_MEMBER_GAVE_IT;
                let unsourced = match self.sourced(id, class, source).await {
                    Ok(Some(Kept::Landed)) => {
                        tracing::error!(target: "ducktape::blob", event = "blob_from_source", ?id, attempts = rounds, reason = missed, "no member gave a blob this node needs over halo, so it took the blob from its source: halo does not reach the members");
                        return Fetched::Landed;
                    }
                    Ok(Some(Kept::Elsewhere)) => None,
                    Ok(None) if every_member_asked => return Fetched::Unheld,
                    Ok(None) => Some("unserved"),
                    Err(unsourced) => Some(unsourced),
                };
                if let Some(unsourced) = unsourced.filter(|_| worth_a_line) {
                    tracing::warn!(target: "ducktape::blob", event = "blob_unsourced", ?id, attempts = rounds, reason = unsourced, "the source did not give a blob this node needs either; it asks again");
                }
            }
            if worth_a_line {
                match rounds {
                    1 => {
                        tracing::warn!(target: "ducktape::blob", event = "blob_unfetched", ?id, attempts = rounds, reason = missed, "no member gave a blob this node needs; it asks again")
                    }
                    _ => {
                        tracing::error!(target: "ducktape::blob", event = "blob_unfetched", ?id, attempts = rounds, reason = missed, "no member gave a blob this node needs; it asks again")
                    }
                }
            }
            tokio::time::sleep(pause(rounds)).await;
        }
    }

    async fn sourced(
        self: &Arc<Self>,
        id: BlobId,
        class: Class,
        source: &impl Source,
    ) -> Result<Option<Kept>, &'static str> {
        let Some(framed) = source.blob(id).await? else {
            return Ok(None);
        };
        self.keep(id, framed, class).await.map(Some)
    }

    async fn seek(self: &Arc<Self>, id: BlobId) -> bool {
        if self.store.has(&id) {
            return true;
        }
        let wanted = self.state().wanted.contains_key(&id);
        if wanted {
            return false;
        }
        let declared = self.declared(&id);
        let Err(missed) = self.round(&id, declared.as_ref(), Class::Client).await else {
            return true;
        };
        let mut state = self.state();
        let fresh = match state.wanted.entry(id) {
            std::collections::btree_map::Entry::Vacant(vacant) => {
                vacant.insert(Wanted {
                    id,
                    declared,
                    due: Instant::now() + pause(1),
                    rounds: 1,
                    pulling: false,
                });
                true
            }
            std::collections::btree_map::Entry::Occupied(_) => false,
        };
        drop(state);
        if fresh {
            tracing::warn!(target: "ducktape::blob", event = "blob_ungathered", ?id, attempts = 1, reason = missed, "no member gave a blob this node wants; the node asks again");
        }
        self.work.notify_waiters();
        false
    }

    fn pulls(&self) -> MutexGuard<'_, Pulls> {
        self.pulls.lock().expect("the pull table is never poisoned")
    }

    fn turn(self: &Arc<Self>, id: &BlobId, class: Class) -> Turn {
        let mut pulls = self.pulls();
        if let Some(owner) = pulls.owners.get(id)
            && owner.class <= class
        {
            return Turn::Joined(owner.ended.subscribe());
        }
        pulls.generations += 1;
        let generation = pulls.generations;
        let ended = watch::Sender::new(());
        let superseded = ended.subscribe();
        let owner = Owner {
            generation,
            class,
            ended,
        };
        pulls.owners.insert(*id, owner);
        pulls.live.insert(generation);
        Turn::Mine(Owning {
            inner: self.clone(),
            id: *id,
            generation,
            superseded,
        })
    }

    async fn seeded(&self, id: &BlobId, generation: u64) -> Result<PathBuf, &'static str> {
        let partial = self.store.partial(id).map_err(|_| "unwritten")?;
        let live = self.pulls().live.clone();
        let seeded = tokio::task::spawn_blocking(move || seed(&partial, generation, &live)).await;
        seeded.map_err(|_| "unwritten")?.map_err(|_| "unwritten")
    }

    async fn asked(
        &self,
        peer: &PublicKey,
        class: Class,
        ask: &Ask,
    ) -> Result<(Answer, Paced<halo::Carried>), &'static str> {
        let Ok(mut stream) = self.lane.open(peer, class, &abi::encode(ask)).await else {
            return Err("unopened");
        };
        let answer = read_answer(&mut stream).await?;
        Ok((answer, stream))
    }

    async fn pull(
        self: &Arc<Self>,
        peer: &PublicKey,
        id: &BlobId,
        declared: Option<&Entry>,
        class: Class,
    ) -> Result<(), &'static str> {
        loop {
            let owning = match self.turn(id, class) {
                Turn::Mine(owning) => owning,
                Turn::Joined(mut pulling) => {
                    closed(&mut pulling).await;
                    if self.store.has(id) {
                        return Ok(());
                    }
                    continue;
                }
            };
            if self.store.has(id) {
                return Ok(());
            }
            let work = tokio::spawn({
                let inner = self.clone();
                let peer = peer.clone();
                let declared = declared.cloned();
                async move { inner.owned(owning, &peer, declared.as_ref(), class).await }
            });
            match work.await.map_err(|_| "unwritten")?? {
                Pulled::Landed => return Ok(()),
                Pulled::Superseded => {}
            }
        }
    }

    async fn owned(
        &self,
        mut owning: Owning,
        peer: &PublicKey,
        declared: Option<&Entry>,
        class: Class,
    ) -> Result<Pulled, &'static str> {
        let id = owning.id;
        let file = self.seeded(&id, owning.generation).await?;
        let pulled = self.pulled(peer, &id, declared, class, &file, &mut owning.superseded);
        let pulled = pulled.await?;
        if let Pulled::Superseded = pulled {
            let _ = tokio::fs::remove_file(&file).await;
        }
        Ok(pulled)
    }

    async fn pulled(
        &self,
        peer: &PublicKey,
        id: &BlobId,
        declared: Option<&Entry>,
        class: Class,
        partial: &Path,
        superseded: &mut watch::Receiver<()>,
    ) -> Result<Pulled, &'static str> {
        if self.store.has(id) {
            return Ok(Pulled::Landed);
        }
        let offset = size_of(partial).map_err(|_| "unread")?.unwrap_or(0);
        let overrun = declared.is_some_and(|entry| offset > entry.len);
        if overrun {
            let _ = tokio::fs::remove_file(partial).await;
            return Err("overrun");
        }
        let ask = Ask { id: *id, offset };
        let Some(asked) = unless_superseded(superseded, self.asked(peer, class, &ask)).await else {
            return Ok(Pulled::Superseded);
        };
        let (answer, mut stream) = asked?;
        let Answer::Have { len } = answer else {
            return Err("lacks");
        };
        let sized = declared.is_none_or(|entry| entry.len == len);
        if !sized {
            return Err("missized");
        }
        if offset > len {
            let _ = tokio::fs::remove_file(partial).await;
            return Err("overrun");
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(partial)
            .await
            .map_err(|_| "unwritten")?;
        let streamed = streamed(&mut file, &mut stream, len - offset, superseded).await;
        let flushed = file.flush().await;
        drop(file);
        let whole = streamed?;
        flushed.map_err(|_| "unwritten")?;
        if !whole {
            return Ok(Pulled::Superseded);
        }
        let refused = match checked(partial, id, declared).await {
            Checked::Named => None,
            Checked::Misnamed => Some("misnamed"),
            Checked::Collision(_) => Some("collision"),
        };
        if let Some(reason) = refused {
            let _ = tokio::fs::remove_file(partial).await;
            return Err(reason);
        }
        adopted(&self.store, id, partial).await?;
        self.landed.send_modify(|landed| *landed += 1);
        Ok(Pulled::Landed)
    }

    async fn keep(
        self: &Arc<Self>,
        id: BlobId,
        framed: Vec<u8>,
        class: Class,
    ) -> Result<Kept, &'static str> {
        let Turn::Mine(mut owning) = self.turn(&id, class) else {
            return Ok(Kept::Elsewhere);
        };
        if self.store.has(&id) {
            return Ok(Kept::Elsewhere);
        }
        let partial = self.store.partial(&id).map_err(|_| "unwritten")?;
        let file = generation_path(&partial, owning.generation);
        let writing = tokio::fs::write(&file, &framed);
        let written = unless_superseded(&mut owning.superseded, writing).await;
        let kept = match written {
            None => Ok(Kept::Elsewhere),
            Some(Err(_)) => Err("unwritten"),
            Some(Ok(())) => self.landing(&id, &file, &owning.superseded).await,
        };
        let landed = kept == Ok(Kept::Landed);
        if !landed {
            let _ = tokio::fs::remove_file(&file).await;
        }
        kept
    }

    async fn landing(
        &self,
        id: &BlobId,
        file: &Path,
        superseded: &watch::Receiver<()>,
    ) -> Result<Kept, &'static str> {
        let refused = match checked(file, id, None).await {
            Checked::Named => None,
            Checked::Misnamed => Some("misnamed"),
            Checked::Collision(_) => Some("collision"),
        };
        if let Some(reason) = refused {
            return Err(reason);
        }
        let taken_over = superseded.has_changed().is_err();
        if taken_over {
            return Ok(Kept::Elsewhere);
        }
        adopted(&self.store, id, file).await?;
        self.landed.send_modify(|landed| *landed += 1);
        Ok(Kept::Landed)
    }

    async fn stage_all(
        &self,
        entries: &[Entry],
        body: &mut (impl AsyncRead + Unpin),
        last_landing: u64,
        uploads: &mut Vec<(BlobId, u64)>,
    ) -> Result<(), Refusal> {
        for entry in entries {
            let upload = self.stage(entry, body, last_landing).await?;
            uploads.extend(upload.map(|upload| (entry.id, upload)));
        }
        let mut past = [0u8; 1];
        let trailing = body.read(&mut past).await.map_err(unread)?;
        if trailing > 0 {
            return Err(Refusal::new(
                reason::INVALID_INPUT,
                "the upload carries bytes past the blobs its put names",
            ));
        }
        Ok(())
    }

    async fn stage(
        &self,
        entry: &Entry,
        body: &mut (impl AsyncRead + Unpin),
        last_landing: u64,
    ) -> Result<Option<u64>, Refusal> {
        let upload = self.uploads.fetch_add(1, Ordering::Relaxed);
        let path = self.store.staging(&entry.id, upload).map_err(unstored)?;
        let (temp, mut file) = tokio::task::spawn_blocking(move || Temp::create(path))
            .await
            .map_err(unstored)?
            .map_err(unstored)?;
        let copied = tokio::io::copy(&mut body.take(entry.len), &mut file).await;
        let flushed = file.flush().await;
        drop(file);
        let whole = copied.is_ok_and(|len| len == entry.len) && flushed.is_ok();
        let checked = match whole {
            true => checked(&temp.path, &entry.id, Some(entry)).await,
            false => Checked::Misnamed,
        };
        let refusal = match checked {
            Checked::Named => None,
            Checked::Misnamed => Some(Refusal::new(
                reason::INVALID_INPUT,
                format!(
                    "the upload does not carry {:?} as a {} blob of {} framed bytes",
                    entry.id, entry.kind, entry.len
                ),
            )),
            Checked::Collision(collision) => Some(collision.into()),
        };
        if let Some(refusal) = refusal {
            return Err(refusal);
        }
        if self.store.has(&entry.id) {
            return Ok(None);
        }
        self.claim(entry.id, upload, last_landing, temp)
            .map_err(unstored)?;
        Ok(Some(upload))
    }
}

async fn streamed(
    file: &mut tokio::fs::File,
    stream: &mut (impl AsyncRead + Unpin),
    mut left: u64,
    superseded: &mut watch::Receiver<()>,
) -> Result<bool, &'static str> {
    let mut chunk = vec![0u8; CHUNK];
    while left > 0 {
        let want = left.min(CHUNK as u64) as usize;
        let reading = stream.read(&mut chunk[..want]);
        let Some(read) = unless_superseded(superseded, reading).await else {
            return Ok(false);
        };
        let read = read.map_err(|_| "broken")?;
        if read == 0 {
            return Err("cut");
        }
        file.write_all(&chunk[..read])
            .await
            .map_err(|_| "unwritten")?;
        left -= read as u64;
    }
    Ok(true)
}

async fn adopted(store: &Blobs, id: &BlobId, file: &Path) -> Result<(), &'static str> {
    let (store, id, file) = (store.clone(), *id, file.to_path_buf());
    let adopted = tokio::task::spawn_blocking(move || store.adopt(&id, &file)).await;
    adopted.map_err(|_| "unwritten")?.map_err(|_| "unwritten")
}

enum Checked {
    Named,
    Misnamed,
    Collision(Collision),
}

async fn checked(path: &Path, id: &BlobId, declared: Option<&Entry>) -> Checked {
    let path = path.to_path_buf();
    let id = *id;
    let declared = declared.cloned();
    let checked = tokio::task::spawn_blocking(move || {
        let header = match names(&path, &id) {
            Ok(Some(header)) => header,
            Err(Error::Collision(collision)) => return Checked::Collision(collision),
            Ok(None) | Err(Error::Io(_) | Error::Corrupt(..)) => return Checked::Misnamed,
        };
        let Some(entry) = declared else {
            return Checked::Named;
        };
        let size = size_of(&path).ok().flatten();
        let matches_entry = size == Some(entry.len) && header.kind == entry.kind;
        match matches_entry {
            true => Checked::Named,
            false => Checked::Misnamed,
        }
    })
    .await;
    checked.unwrap_or(Checked::Misnamed)
}

async fn gathering(inner: Arc<Inner>) {
    loop {
        let notified = inner.work.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        match inner.next() {
            Next::Pull(id, declared) => {
                let round = inner.round(&id, declared.as_ref(), Class::Member).await;
                inner.gathered(&id, round);
            }
            Next::Until(at) => {
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep_until(at) => {}
                }
            }
            Next::Idle => notified.await,
        }
    }
}

async fn serving(inner: Arc<Inner>, serving: NonZeroUsize) {
    let serving = Arc::new(Semaphore::new(serving.get()));
    loop {
        let Ok(permit) = serving.clone().acquire_owned().await else {
            return;
        };
        let Some(inbound) = inner.lane.accept().await else {
            return;
        };
        let store = inner.store.clone();
        tokio::spawn(async move {
            let from = inbound.from.clone();
            let answered = answer(&store, inbound).await;
            if let Err(reason) = answered {
                tracing::debug!(target: "ducktape::blob", %from, reason, "the node did not give a blob");
            }
            drop(permit);
        });
    }
}

async fn read_answer<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Answer, &'static str> {
    let len = stream.read_u32_le().await.map_err(|_| "cut")?;
    let mut bytes = Vec::new();
    let read = stream
        .take(u64::from(len))
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "cut")?;
    let whole = read == len as usize;
    if !whole {
        return Err("cut");
    }
    abi::decode(&bytes).map_err(|_| "malformed")
}

async fn write_answer<S: AsyncWrite + Unpin>(
    stream: &mut S,
    answer: &Answer,
) -> Result<(), &'static str> {
    let bytes = abi::encode(answer);
    stream
        .write_u32_le(bytes.len() as u32)
        .await
        .map_err(|_| "broken")?;
    stream.write_all(&bytes).await.map_err(|_| "broken")
}

async fn answer<S: AsyncRead + AsyncWrite + Unpin>(
    store: &Blobs,
    inbound: Inbound<S>,
) -> Result<(), &'static str> {
    let Inbound {
        meta, mut stream, ..
    } = inbound;
    let ask: Ask = abi::decode(&meta).map_err(|_| "malformed")?;
    let (id, offset) = (ask.id, ask.offset);
    let reading = store.clone();
    let opened = tokio::task::spawn_blocking(move || reading.reader(&id, offset)).await;
    let opened = opened.map_err(|_| "unread")?.map_err(|_| "unread")?;
    let Some(mut reader) = opened else {
        write_answer(&mut stream, &Answer::Lacks).await?;
        return Err("lacks");
    };
    let len = reader.framed_len();
    if offset > len {
        write_answer(&mut stream, &Answer::Lacks).await?;
        return Err("offset_past_end");
    }
    write_answer(&mut stream, &Answer::Have { len }).await?;
    loop {
        let read = tokio::task::spawn_blocking(move || {
            let piece = reader.piece();
            (piece, reader)
        });
        let (piece, back) = read.await.map_err(|_| "unread")?;
        reader = back;
        let Some(piece) = piece.map_err(|_| "unread")? else {
            break;
        };
        stream.write_all(&piece).await.map_err(|_| "broken")?;
    }
    stream.shutdown().await.map_err(|_| "broken")
}

#[cfg(test)]
mod tests;
