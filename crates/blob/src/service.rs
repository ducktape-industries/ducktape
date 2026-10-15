// How a node gathers every bundle the blobs program records, serves blobs to members over
// halo and receipts what it holds. noded starts it, tells it each applied block and asks it
// for a blob it lacks.

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use abi::{BlobId, Refusal, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::ed25519::PublicKey;
use halo::{Class, Halo, Inbound, Links, Paced, Streams};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncSeekExt as _, AsyncWrite, AsyncWriteExt as _};
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
    entry: Entry,
    due: Instant,
    rounds: u32,
    pulling: bool,
}

enum Next {
    Pull(Entry),
    Until(Instant),
    Idle,
}

#[derive(Default)]
struct State {
    open: BTreeMap<u64, Open>,
    wanted: BTreeMap<BlobId, Wanted>,
    senders: HashMap<BlobId, SentBy>,
}

struct Inner {
    halo: Arc<Halo>,
    lane: Streams<Arc<Links>>,
    store: Blobs,
    state: Mutex<State>,
    work: Notify,
    pulls: Mutex<HashMap<BlobId, Arc<tokio::sync::Mutex<()>>>>,
    uploads: AtomicU64,
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
            pulls: Mutex::default(),
            uploads: AtomicU64::new(0),
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
        if let Some(open) = self.inner.state().open.get_mut(&bundle) {
            open.posted = Some(receipt);
        }
    }

    pub async fn fetch(&self, id: BlobId, class: Class) {
        self.inner.fetch(id, class).await
    }

    pub async fn stage(
        &self,
        entries: &[Entry],
        body: &mut (impl AsyncRead + Unpin),
    ) -> Result<(), Refusal> {
        for entry in entries {
            self.inner.stage(entry, body).await?;
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

    pub fn store(&self) -> &Blobs {
        &self.inner.store
    }

    pub fn landed(&self) -> watch::Receiver<u64> {
        self.inner.landed.subscribe()
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

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("the blob state lock is never poisoned")
    }

    fn local(&self, entry: &Entry) -> Local {
        let staged = self.store.staged(&entry.id);
        let in_staging = !self.store.has(&entry.id) && staged.is_file();
        if in_staging && self.store.adopt(&entry.id, &staged).is_err() {
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
            state.wanted.entry(entry.id).or_insert(Wanted {
                entry,
                due: now,
                rounds: 0,
                pulling: false,
            });
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
                return Next::Pull(want.entry.clone());
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
            tracing::warn!(target: "ducktape::blob", event = "blob_ungathered", ?id, attempts = rounds, reason = missed, "no member gave a blob a bundle names; the node asks again");
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
        &self,
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

    async fn fetch(&self, id: BlobId, class: Class) {
        let declared = self.declared(&id);
        let mut rounds = 0u32;
        loop {
            if self.store.has(&id) {
                return;
            }
            let Err(missed) = self.round(&id, declared.as_ref(), class).await else {
                return;
            };
            rounds += 1;
            let worth_a_line = rounds == 1 || rounds.is_multiple_of(10);
            if worth_a_line {
                tracing::warn!(target: "ducktape::blob", event = "blob_unfetched", ?id, attempts = rounds, reason = missed, "no member gave a blob this node needs; it asks again");
            }
            tokio::time::sleep(pause(rounds)).await;
        }
    }

    fn pull_lock(&self, id: &BlobId) -> Arc<tokio::sync::Mutex<()>> {
        let mut pulls = self.pulls.lock().expect("the pull lock is never poisoned");
        pulls.retain(|_, lock| Arc::strong_count(lock) > 1);
        pulls.entry(*id).or_default().clone()
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
        &self,
        peer: &PublicKey,
        id: &BlobId,
        declared: Option<&Entry>,
        class: Class,
    ) -> Result<(), &'static str> {
        let lock = self.pull_lock(id);
        let _pulling = lock.lock().await;
        if self.store.has(id) {
            return Ok(());
        }
        let partial = self.store.partial(id).map_err(|_| "unwritten")?;
        let offset = size_of(&partial).map_err(|_| "unread")?.unwrap_or(0);
        let overrun = declared.is_some_and(|entry| offset > entry.len);
        if overrun {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err("overrun");
        }
        let (answer, mut stream) = self.asked(peer, class, &Ask { id: *id, offset }).await?;
        let Answer::Have { len } = answer else {
            return Err("lacks");
        };
        let sized = declared.is_none_or(|entry| entry.len == len);
        if !sized {
            return Err("missized");
        }
        if offset > len {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err("overrun");
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&partial)
            .await
            .map_err(|_| "unwritten")?;
        let mut left = len - offset;
        let mut chunk = vec![0u8; CHUNK];
        while left > 0 {
            let want = left.min(CHUNK as u64) as usize;
            let read = stream
                .read(&mut chunk[..want])
                .await
                .map_err(|_| "broken")?;
            if read == 0 {
                return Err("cut");
            }
            file.write_all(&chunk[..read])
                .await
                .map_err(|_| "unwritten")?;
            left -= read as u64;
        }
        file.flush().await.map_err(|_| "unwritten")?;
        drop(file);
        let refused = match checked(&partial, id, declared).await {
            Checked::Named => None,
            Checked::Misnamed => Some("misnamed"),
            Checked::Collision(_) => Some("collision"),
        };
        if let Some(reason) = refused {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(reason);
        }
        self.store.adopt(id, &partial).map_err(|_| "unwritten")?;
        self.landed.send_modify(|landed| *landed += 1);
        Ok(())
    }

    async fn stage(
        &self,
        entry: &Entry,
        body: &mut (impl AsyncRead + Unpin),
    ) -> Result<(), Refusal> {
        let upload = self.uploads.fetch_add(1, Ordering::Relaxed);
        let temp = self.store.staging(&entry.id, upload).map_err(unstored)?;
        let mut file = tokio::fs::File::create(&temp).await.map_err(unstored)?;
        let copied = tokio::io::copy(&mut body.take(entry.len), &mut file).await;
        let flushed = file.flush().await;
        drop(file);
        let whole = copied.is_ok_and(|len| len == entry.len) && flushed.is_ok();
        let checked = match whole {
            true => checked(&temp, &entry.id, Some(entry)).await,
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
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(refusal);
        }
        tokio::fs::rename(&temp, self.store.staged(&entry.id))
            .await
            .map_err(unstored)
    }
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
            Next::Pull(entry) => {
                let round = inner.round(&entry.id, Some(&entry), Class::Member).await;
                inner.gathered(&entry.id, round);
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
    let opened = store.file(&ask.id).map_err(|_| "unread")?;
    let Some(file) = opened else {
        write_answer(&mut stream, &Answer::Lacks).await?;
        return Err("lacks");
    };
    let mut file = tokio::fs::File::from_std(file);
    let len = file.metadata().await.map_err(|_| "unread")?.len();
    if ask.offset > len {
        write_answer(&mut stream, &Answer::Lacks).await?;
        return Err("offset_past_end");
    }
    write_answer(&mut stream, &Answer::Have { len }).await?;
    file.seek(std::io::SeekFrom::Start(ask.offset))
        .await
        .map_err(|_| "unread")?;
    let mut left = (&mut file).take(len - ask.offset);
    tokio::io::copy(&mut left, &mut stream)
        .await
        .map_err(|_| "broken")?;
    stream.shutdown().await.map_err(|_| "broken")
}
