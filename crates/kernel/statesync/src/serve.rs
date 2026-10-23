use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use abi::{ProgramId, Refusal, reason};
use arc_swap::ArcSwap;
use commonware_codec::{DecodeExt as _, Encode as _, EncodeSize as _};
use commonware_consensus::types::Height;
use commonware_runtime::ReadOptions;
use commonware_storage::journal::contiguous::Contiguous as _;
use commonware_storage::merkle;
use commonware_storage::qmdb::{self, sync::Source as _};
use consensus::MarshalMailbox;
use futures::StreamExt as _;
use host::Tip;
use node::Node;
use state::Serving;

use crate::Anchor;
use crate::wire::{Head, Manifest, Request, Response};
use crate::{Context, SyncRequest, SyncResponse, SyncTarget};

pub trait Anchors: Send + Sync {
    fn anchor(&self, tip: Tip) -> impl Future<Output = Option<Anchor>> + Send;
}

impl Anchors for MarshalMailbox {
    async fn anchor(&self, tip: Tip) -> Option<Anchor> {
        if tip.height == 0 {
            return self.get_block(Height::zero()).await.map(Anchor::Genesis);
        }
        self.get_finalization(Height::new(tip.height))
            .await
            .map(Anchor::Finalized)
    }
}

pub struct Published<E: Context> {
    tip: Tip,
    anchor: Option<Anchor>,
    programs: BTreeMap<ProgramId, Arc<Serving<E>>>,
}

impl<E: Context> Anchors for Published<E> {
    async fn anchor(&self, tip: Tip) -> Option<Anchor> {
        let ours = tip == self.tip;
        ours.then(|| self.anchor.clone()).flatten()
    }
}

impl<E: Context> Published<E> {
    pub fn of(node: &Node<E>, anchor: Option<Anchor>) -> node::Result<Published<E>> {
        let programs = node
            .host()
            .store()
            .commitments()
            .map(|(program, commitment)| (program.clone(), commitment.serving()))
            .collect();
        Ok(Published {
            tip: node.tip()?,
            anchor,
            programs,
        })
    }

    pub fn tip(&self) -> Tip {
        self.tip
    }

    pub fn serving(&self, program: &str) -> Option<&Arc<Serving<E>>> {
        self.programs.get(program)
    }
}

pub struct Holds<E: Context> {
    held: Arc<ArcSwap<Vec<Weak<Pinned<E>>>>>,
    numbered: Arc<AtomicU64>,
}

/// A held head's programs a joiner has not synced yet, by the hold's number.
struct Pinned<E: Context> {
    number: u64,
    programs: Mutex<BTreeMap<ProgramId, Arc<Serving<E>>>>,
}

impl<E: Context> Pinned<E> {
    fn programs(&self) -> std::sync::MutexGuard<'_, BTreeMap<ProgramId, Arc<Serving<E>>>> {
        self.programs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<E: Context> Default for Holds<E> {
    fn default() -> Holds<E> {
        Holds {
            held: Arc::default(),
            numbered: Arc::default(),
        }
    }
}

impl<E: Context> Clone for Holds<E> {
    fn clone(&self) -> Holds<E> {
        Holds {
            held: self.held.clone(),
            numbered: self.numbered.clone(),
        }
    }
}

impl<E: Context> Holds<E> {
    pub fn hold(&self, published: &Published<E>) -> Hold<E> {
        let pinned = Arc::new(Pinned {
            number: self.numbered.fetch_add(1, Ordering::Relaxed),
            programs: Mutex::new(published.programs.clone()),
        });
        let mine = Arc::downgrade(&pinned);
        self.held.rcu(|held| {
            let mut kept: Vec<_> = held
                .iter()
                .filter(|other| other.strong_count() > 0)
                .cloned()
                .collect();
            kept.push(mine.clone());
            kept
        });
        Hold {
            pinned,
            holds: self.clone(),
        }
    }

    pub fn serving(&self, hold: u64, program: &str) -> Option<Arc<Serving<E>>> {
        self.held
            .load()
            .iter()
            .filter_map(Weak::upgrade)
            .find(|pinned| pinned.number == hold)?
            .programs()
            .get(program)
            .cloned()
    }

    pub fn release(&self, hold: u64, program: &str) {
        let pinned = self
            .held
            .load()
            .iter()
            .filter_map(Weak::upgrade)
            .find(|pinned| pinned.number == hold);
        let Some(pinned) = pinned else {
            return;
        };
        pinned.programs().remove(program);
    }

    fn covering(&self, program: &str, request: &SyncRequest) -> Option<Arc<Serving<E>>> {
        self.held
            .load()
            .iter()
            .filter_map(Weak::upgrade)
            .find_map(|pinned| {
                let programs = pinned.programs();
                let serving = programs.get(program)?;
                let bounds = serving.reader.bounds();
                let covers = bounds.start <= *request.start() && *request.size() <= bounds.end;
                covers.then(|| serving.clone())
            })
    }
}

pub struct Hold<E: Context> {
    pinned: Arc<Pinned<E>>,
    holds: Holds<E>,
}

impl<E: Context> Hold<E> {
    pub fn number(&self) -> u64 {
        self.pinned.number
    }
}

impl<E: Context> Drop for Hold<E> {
    fn drop(&mut self) {
        let mine = Arc::downgrade(&self.pinned);
        self.holds.held.rcu(|held| {
            held.iter()
                .filter(|other| other.strong_count() > 0 && !other.ptr_eq(&mine))
                .cloned()
                .collect::<Vec<_>>()
        });
    }
}

/// What one sync answer reads at most: operations up to `bytes`, for up to `time`.
#[derive(Clone, Copy, Debug)]
pub struct AnswerBounds {
    pub bytes: usize,
    pub time: Duration,
}

impl Default for AnswerBounds {
    fn default() -> AnswerBounds {
        AnswerBounds {
            bytes: abi::BLOCK_BYTES,
            time: Duration::from_secs(10),
        }
    }
}

pub enum Served {
    Answered(Response),
    Unanchored {
        tip: Tip,
        targets: Result<BTreeMap<ProgramId, SyncTarget>, Refusal>,
    },
}

pub async fn serve<E: Context>(
    published: &Published<E>,
    holds: &Holds<E>,
    request: Request,
    bounds: AnswerBounds,
) -> Served {
    match request {
        Request::Head => head(published),
        Request::Sync { program, request } => {
            Served::Answered(sync(published, holds, &program, &request, bounds).await)
        }
        Request::Release { hold, program } => {
            holds.release(hold, &program);
            Served::Answered(Response::Released)
        }
    }
}

pub async fn anchored<A: Anchors>(served: Served, anchors: &A) -> Response {
    let (tip, targets) = match served {
        Served::Answered(response) => return response,
        Served::Unanchored { tip, targets } => (tip, targets),
    };
    let Some(anchor) = anchors.anchor(tip).await else {
        return refused(reason::NOT_FOUND, "the tip has no anchor to start from");
    };
    match targets {
        Ok(targets) => Response::Head(Head {
            tip,
            anchor,
            targets,
        }),
        Err(refusal) => Response::Refused(refusal),
    }
}

pub fn held<E: Context>(
    published: Arc<Published<E>>,
    holds: &Holds<E>,
) -> (Served, Option<Hold<E>>) {
    let served = head(&published);
    let headed = matches!(&served, Served::Unanchored { targets: Ok(_), .. });
    let hold = headed.then(|| holds.hold(&published));
    (served, hold)
}

fn head<E: Context>(published: &Published<E>) -> Served {
    Served::Unanchored {
        tip: published.tip,
        targets: targets(published),
    }
}

pub fn manifest<E: Context>(node: &Node<E>) -> Result<Manifest, Refusal> {
    let published = Published::of(node, None)
        .map_err(|error| Refusal::new(reason::NOT_FOUND, error.to_string()))?;
    let targets = targets(&published)?;
    Ok(Manifest {
        tip: published.tip,
        targets,
    })
}

fn targets<E: Context>(
    published: &Published<E>,
) -> Result<BTreeMap<ProgramId, SyncTarget>, Refusal> {
    let mut targets = BTreeMap::new();
    for (program, serving) in &published.programs {
        let Some(target) = &serving.target else {
            let sentence = format!("{program} has nothing to sync");
            return Err(Refusal::new(reason::NOT_FOUND, sentence));
        };
        targets.insert(program.clone(), target.clone());
    }
    Ok(targets)
}

async fn sync<E: Context>(
    published: &Published<E>,
    holds: &Holds<E>,
    program: &str,
    request: &[u8],
    bounds: AnswerBounds,
) -> Response {
    let Some(newest) = published.programs.get(program) else {
        return refused(
            reason::UNKNOWN_PROGRAM,
            "the program is not on this network",
        );
    };
    let request = match SyncRequest::decode(request) {
        Ok(request) => request,
        Err(error) => return refused(reason::PROTOCOL, error),
    };
    let retained = newest.reader.bounds().start;
    let serving = if *request.start() < retained {
        holds.covering(program, &request)
    } else {
        Some(newest.clone())
    };
    let Some(serving) = serving else {
        tracing::debug!(
            target: "ducktape::sync",
            event = "sync_refused",
            program,
            reason = "pruned",
            start = *request.start(),
            retained,
        );
        return refused(
            reason::STALE,
            "this node pruned the operations the request starts at; read the head again",
        );
    };
    match answer(&serving, request, bounds).await {
        Ok(response) => Response::Sync(response.encode().to_vec()),
        Err(error) => refused(reason::INVALID_INPUT, error),
    }
}

async fn answer<E: Context>(
    serving: &Serving<E>,
    request: SyncRequest,
    bounds: AnswerBounds,
) -> Result<SyncResponse, qmdb::Error<state::Family>> {
    let SyncRequest::Operations {
        size,
        start,
        max_ops,
    } = request
    else {
        let (response, _) = serving.reader.serve(request).await?;
        return Ok(response);
    };
    let past_the_log = size > serving.reader.size();
    if past_the_log {
        return Err(merkle::Error::RangeOutOfBounds(size).into());
    }
    let past_the_size = start >= size;
    if past_the_size {
        return Err(merkle::Error::RangeOutOfBounds(start).into());
    }
    let inactive_peaks = qmdb::inactive_peaks_at(&serving.reader, size).await?;
    let wanted = max_ops.get().min(*size - *start);
    let operations = read_within(&serving.reader, *start, wanted, bounds).await?;
    let end = start.saturating_add(operations.len() as u64);
    let proof = serving
        .reader
        .historical_range_proof(size, start..end, inactive_peaks)
        .await?;
    Ok(SyncResponse::Operations { proof, operations })
}

async fn read_within<E: Context>(
    reader: &state::Reader<E>,
    start: u64,
    wanted: u64,
    bounds: AnswerBounds,
) -> Result<Vec<state::Op>, qmdb::Error<state::Family>> {
    let begun = Instant::now();
    let replay_buffer = state::Tuning::default().replay_buffer;
    let replayed = reader
        .replay(start, replay_buffer, ReadOptions::default())
        .await?;
    let mut replayed = std::pin::pin!(replayed);
    let mut operations = Vec::new();
    let mut bytes = 0usize;
    while let Some(read) = replayed.next().await {
        let (_, operation) = read?;
        bytes = bytes.saturating_add(operation.encode_size());
        operations.push(operation);
        let holds_all_wanted = operations.len() as u64 == wanted;
        let fills_the_bytes = bytes >= bounds.bytes;
        let out_of_time = begun.elapsed() >= bounds.time;
        let answered = holds_all_wanted || fills_the_bytes || out_of_time;
        if answered {
            break;
        }
    }
    Ok(operations)
}

fn refused(reason: &str, sentence: impl ToString) -> Response {
    Response::Refused(Refusal::new(reason, sentence.to_string()))
}
