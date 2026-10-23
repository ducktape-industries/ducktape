use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use abi::{ProgramId, Refusal, reason};
use arc_swap::ArcSwap;
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_consensus::types::Height;
use commonware_storage::journal::contiguous::Contiguous as _;
use commonware_storage::qmdb::sync::Source as _;
use consensus::MarshalMailbox;
use host::Tip;
use node::Node;
use state::Serving;

use crate::Anchor;
use crate::wire::{Head, Manifest, Request, Response};
use crate::{Context, SyncRequest, SyncTarget};

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
) -> Served {
    match request {
        Request::Head => head(published),
        Request::Sync { program, request } => {
            Served::Answered(sync(published, holds, &program, &request).await)
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
    match serving.reader.serve(request).await {
        Ok((response, _)) => Response::Sync(response.encode().to_vec()),
        Err(error) => refused(reason::INVALID_INPUT, error),
    }
}

fn refused(reason: &str, sentence: impl ToString) -> Response {
    Response::Refused(Refusal::new(reason, sentence.to_string()))
}
