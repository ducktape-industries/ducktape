mod bulk;
pub mod lane;
mod remote;
mod serve;
mod wire;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::path::Path;
use std::time::Duration;

use abi::{BlobId, ProgramId, Refusal, reason};
use commonware_consensus::simplex::scheme::ed25519::Scheme;
use commonware_parallel::Sequential;
use commonware_storage::qmdb::sync;
use consensus::{Certificate, validators_of};
use futures::{Stream, StreamExt as _};
use node::{Digest, Node, Synced};
use state::{Adoption, Commitment, SyncFailure, SyncTarget, commitment_name};

pub use bulk::{Bulk, HeldClosing, bulk_join, bulk_readopt};
pub use consensus::Anchor;
pub use consensus::Context;
pub use remote::Remote;
pub use serve::{Anchors, Hold, Holds, Published, Served, anchored, held, manifest, serve};
pub use wire::{Head, Manifest, Request, Response};

pub type SyncRequest = sync::Request<state::Family>;
pub type SyncResponse = sync::Response<state::Family, state::Op, Digest>;

const ATTEMPTS_PER_LINE: u64 = 10;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Host(#[from] host::Error),
    #[error(transparent)]
    Node(#[from] node::Error),
    #[error(transparent)]
    State(#[from] state::Error),
    #[error("the exchange failed: {0}")]
    Exchange(Box<dyn std::error::Error + Send + Sync>),
    #[error("the peer refused: {}: {}", .0.reason, .0.sentence)]
    Refused(Refusal),
    #[error("the peer answered another request: {0:?}")]
    Answer(Box<Response>),
    #[error("the peer's answer does not decode: {0}")]
    Codec(#[from] commonware_codec::Error),
    #[error("the peer's head names a tip its anchor and state do not")]
    Head,
    #[error(
        "epoch {epoch} does not verify against the trusted validators; needs a newer checkpoint"
    )]
    Untrusted { epoch: u64 },
    #[error("the adopted root {actual:?} differs from the finalized root {expected:?}")]
    Root {
        expected: abi::Root,
        actual: abi::Root,
    },
    #[error("the adopted state belongs to another network")]
    Network,
    #[error(
        "the tip's certificate does not verify against the validators seated for epoch {epoch}"
    )]
    Certificate { epoch: u64 },
    #[error("the tip's certificate names epoch {epoch} and the state records no validators for it")]
    Unrecorded { epoch: u64 },
    #[error("the state names blob {0:?} and the peer does not serve it")]
    Blob(BlobId),
    #[error("the peer answered {answered} blobs of the {asked} asked")]
    Blobs { asked: usize, answered: usize },
    #[error("blob {0:?} does not install: {1}")]
    Install(BlobId, Box<dyn std::error::Error + Send + Sync>),
}

pub type Result<T> = std::result::Result<T, Error>;

pub type Holding = Box<dyn std::any::Any + Send>;

pub type Ending = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

/// A source's head and the number of the hold it keeps on it while `ending` is pending.
pub struct Held {
    pub head: Response,
    pub hold: Option<u64>,
    pub ending: Ending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Transient(&'static str),
    Definite,
}

pub trait Fault: std::error::Error + Send + Sync + 'static {
    fn failure(&self) -> Failure;
}

impl Fault for Infallible {
    fn failure(&self) -> Failure {
        match *self {}
    }
}

pub trait Exchange: Clone + Send + Sync + 'static {
    type Error: Fault;

    fn exchange(
        &self,
        request: Request,
    ) -> impl Future<Output = std::result::Result<Response, Self::Error>> + Send;

    fn hold(&self) -> impl Future<Output = std::result::Result<Held, Self::Error>> + Send {
        async move {
            let head = self.exchange(Request::Head).await?;
            let unheld: Ending = Box::pin(futures::future::pending());
            Ok(Held {
                head,
                hold: None,
                ending: unheld,
            })
        }
    }

    fn stream_blobs(
        &self,
        ids: Vec<BlobId>,
    ) -> impl Stream<Item = std::result::Result<Option<Vec<u8>>, Self::Error>> + Send;
}

pub trait Install: Send {
    fn install(&mut self, id: BlobId, framed: Vec<u8>) -> impl Future<Output = Result<()>> + Send;
}

pub trait Place: Send {
    type Error: Fault;

    fn place(
        &mut self,
        name: &str,
    ) -> impl Future<Output = std::result::Result<Option<Head>, Self::Error>> + Send;

    fn discard(&mut self) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send;
}

pub struct Unplaced;

impl Place for Unplaced {
    type Error = Infallible;

    async fn place(&mut self, _: &str) -> std::result::Result<Option<Head>, Infallible> {
        Ok(None)
    }

    async fn discard(&mut self) -> std::result::Result<(), Infallible> {
        Ok(())
    }
}

pub struct Joined<E: Context> {
    pub node: Node<E>,
    pub anchor: Anchor,
}

pub async fn join<E: Context, X: Exchange>(
    context: E,
    name: &str,
    dir: &Path,
    network: Vec<u8>,
    pause: Duration,
    exchange: X,
) -> Result<Joined<E>> {
    join_with(context, name, dir, network, pause, exchange, Unplaced).await
}

pub async fn join_with<E: Context, X: Exchange, P: Place>(
    context: E,
    name: &str,
    dir: &Path,
    network: Vec<u8>,
    pause: Duration,
    exchange: X,
    place: P,
) -> Result<Joined<E>> {
    let asking = Asking {
        context: context.child("ask"),
        exchange,
        pause,
    };
    let placed = placed(&context, name, &asking, place).await?;
    adopting(
        context,
        name,
        Over::Dir(dir),
        Adoption::default(),
        network,
        &asking,
        None,
        placed,
    )
    .await
}

pub async fn ahead<E: Context, X: Exchange>(
    context: E,
    pause: Duration,
    exchange: X,
    past: u64,
) -> Result<()> {
    let asking = Asking {
        context,
        exchange,
        pause,
    };
    let mut now = crate::head(&asking).await?;
    while now.tip.height <= past {
        asking.context.sleep(asking.pause).await;
        now = crate::head(&asking).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn readopt<E: Context, X: Exchange>(
    context: E,
    name: &str,
    parts: host::Parts,
    adoption: Adoption,
    network: Vec<u8>,
    pause: Duration,
    exchange: X,
    past: u64,
) -> Result<Joined<E>> {
    let asking = Asking {
        context: context.child("ask"),
        exchange,
        pause,
    };
    adopting(
        context,
        name,
        Over::Parts(parts),
        adoption,
        network,
        &asking,
        Some(past),
        None,
    )
    .await
}

enum Over<'a> {
    Dir(&'a Path),
    Parts(host::Parts),
}

impl Over<'_> {
    fn opened(&mut self) -> Result<host::Parts> {
        let parts = match self {
            Over::Dir(dir) => host::Parts::open(dir)?,
            Over::Parts(parts) => return Ok(parts.clone()),
        };
        *self = Over::Parts(parts.clone());
        Ok(parts)
    }
}

#[allow(clippy::too_many_arguments)]
async fn adopting<E: Context, X: Exchange>(
    mut context: E,
    name: &str,
    mut over: Over<'_>,
    mut adoption: Adoption,
    network: Vec<u8>,
    asking: &Asking<E, X>,
    past: Option<u64>,
    mut placed: Option<Placed<E>>,
) -> Result<Joined<E>> {
    let mut retargets: u64 = 0;
    let mut fetched = BTreeSet::new();
    loop {
        let (head, commitments) = match placed.take() {
            Some(placed) => placed,
            None => adopted_head(&context, name, asking, past).await?,
        };
        let synced = Synced {
            height: head.tip.height,
            commitments,
        };
        let mut node = Node::adopt_over(
            context.child("node"),
            name,
            over.opened()?,
            synced,
            &mut adoption,
        )
        .await?;
        let state_names_this_network = node.network() == network;
        if !state_names_this_network {
            return Err(Error::Network);
        }
        let state_records_the_tip = node.tip()? == head.tip;
        if !state_records_the_tip {
            return Err(Error::Head);
        }
        if let Anchor::Finalized(certificate) = &head.anchor {
            verified(&mut context, &network, &node, certificate)?;
        }
        let code = unheld_code(&node)?;
        let mut joining = Joining {
            node: &mut node,
            fetched: &mut fetched,
        };
        let unserved = installed(asking, code, &mut joining).await?;
        let Some(&blob) = unserved.first() else {
            if retargets > 0 {
                node.host().drop_unnamed(&fetched, node.now()).await?;
            }
            return Ok(Joined {
                node,
                anchor: head.anchor,
            });
        };
        let now = crate::head(asking).await?;
        let source_moved = now.tip != head.tip;
        if !source_moved {
            return Err(Error::Blob(blob));
        }
        drop(node);
        retargets += 1;
        tracing::info!(
            target: "ducktape::sync",
            event = "join_retargeted",
            reason = "blob_released",
            height = head.tip.height,
            unserved = unserved.len(),
            rewritten = adoption.replaced().len(),
            retargets,
            "the source no longer serves code the adopted state runs; it reads the head again"
        );
    }
}

pub async fn fill<E: Context, X: Exchange>(
    context: E,
    pause: Duration,
    exchange: X,
    missing: Vec<BlobId>,
    install: &mut impl Install,
) -> Result<Vec<BlobId>> {
    let asking = Asking {
        context,
        exchange,
        pause,
    };
    installed(&asking, missing, install).await
}

type Placed<E> = (Head, BTreeMap<ProgramId, Commitment<E>>);

async fn placed<E: Context, X: Exchange, P: Place>(
    context: &E,
    name: &str,
    asking: &Asking<E, X>,
    mut place: P,
) -> Result<Option<Placed<E>>> {
    let mut attempts: u64 = 0;
    let head = loop {
        let error = match place.place(name).await {
            Ok(Some(head)) => break head,
            Ok(None) => return Ok(None),
            Err(error) => error,
        };
        let Failure::Transient(reason) = error.failure() else {
            let unplaced = Unmatched {
                reason: "snapshot_unplaced",
                sentence: error.to_string(),
            };
            return refused(&mut place, unplaced).await;
        };
        asking.waited(reason, &mut attempts).await;
    };
    let anchor_names_the_tip = head.anchor.names(head.tip);
    if !anchor_names_the_tip {
        let unanchored = Unmatched {
            reason: "snapshot_unanchored",
            sentence: format!(
                "the snapshot's anchor does not name its tip at {}",
                head.tip.height
            ),
        };
        return refused(&mut place, unanchored).await;
    }
    let commitments = match opened(context, name, &head.targets).await {
        Ok(commitments) => commitments,
        Err(unmatched) => return refused(&mut place, unmatched).await,
    };
    tracing::info!(
        target: "ducktape::sync",
        event = "join_snapshot_opened",
        height = head.tip.height,
        programs = commitments.len(),
        "every commitment of the source's snapshot opens at the target its head names; the join adopts them"
    );
    Ok(Some((head, commitments)))
}

struct Unmatched {
    reason: &'static str,
    sentence: String,
}

async fn refused<P: Place, T>(place: &mut P, unmatched: Unmatched) -> Result<Option<T>> {
    tracing::warn!(
        target: "ducktape::sync",
        event = "join_snapshot_refused",
        reason = unmatched.reason,
        sentence = %unmatched.sentence,
        "the join refuses its source's snapshot, removes what it placed and syncs the state by proof"
    );
    place
        .discard()
        .await
        .map_err(|error| Error::Exchange(Box::new(error)))?;
    Ok(None)
}

async fn opened<E: Context>(
    context: &E,
    name: &str,
    targets: &BTreeMap<ProgramId, SyncTarget>,
) -> std::result::Result<BTreeMap<ProgramId, Commitment<E>>, Unmatched> {
    let mut commitments = BTreeMap::new();
    for (program, target) in targets {
        let unopened = |error: state::Error| Unmatched {
            reason: "snapshot_unopened",
            sentence: format!("{program}: {error}"),
        };
        let commitment = Commitment::open(
            context
                .child("snapshot")
                .with_attribute("program", program.as_str()),
            &commitment_name(name, program),
            0,
        )
        .await
        .map_err(unopened)?;
        let opened_at = commitment.target().map_err(unopened)?;
        let at_the_heads_target = opened_at.as_ref() == Some(target);
        if !at_the_heads_target {
            return Err(Unmatched {
                reason: "snapshot_target",
                sentence: format!(
                    "{program} opens at {opened_at:?}, and the snapshot's head names {target:?}"
                ),
            });
        }
        commitments.insert(program.clone(), commitment);
    }
    Ok(commitments)
}

async fn adopted_head<E: Context, X: Exchange>(
    context: &E,
    name: &str,
    asking: &Asking<E, X>,
    past: Option<u64>,
) -> Result<(Head, BTreeMap<ProgramId, Commitment<E>>)> {
    let mut retargets: u64 = 0;
    let mut ended: u64 = 0;
    loop {
        let (head, number, hold) = held_head(asking).await?;
        let anchor_names_the_tip = head.anchor.names(head.tip);
        if !anchor_names_the_tip {
            return Err(Error::Head);
        }
        let behind = past.is_some_and(|past| head.tip.height <= past);
        if behind {
            asking.context.sleep(asking.pause).await;
            continue;
        }
        let syncing = synced_to(context, name, asking, &head.targets, number);
        let Some(synced) = while_held(syncing, hold).await else {
            retargets += 1;
            ended += 1;
            if ended.is_power_of_two() {
                tracing::info!(
                    target: "ducktape::sync",
                    event = "join_retargeted",
                    reason = "hold_ended",
                    height = head.tip.height,
                    retargets,
                    ended,
                    "the source's hold on the head this join synced to ended; it reads the head again under a new hold"
                );
            }
            asking.context.sleep(asking.pause).await;
            continue;
        };
        let error = match synced {
            Ok(commitments) => return Ok((head, commitments)),
            Err(error) => error,
        };
        if !pruned_past(&error) {
            return Err(error);
        }
        retargets += 1;
        tracing::info!(
            target: "ducktape::sync",
            event = "join_retargeted",
            reason = "source_pruned",
            height = head.tip.height,
            retargets,
            "the source pruned past the head this join synced to; it reads the head again"
        );
    }
}

async fn while_held<T>(work: impl Future<Output = T>, hold: Ending) -> Option<T> {
    let work = std::pin::pin!(work);
    match futures::future::select(work, hold).await {
        futures::future::Either::Left((done, _)) => Some(done),
        futures::future::Either::Right(_) => None,
    }
}

async fn synced_to<E: Context, X: Exchange>(
    context: &E,
    name: &str,
    asking: &Asking<E, X>,
    targets: &BTreeMap<ProgramId, SyncTarget>,
    hold: Option<u64>,
) -> Result<BTreeMap<ProgramId, Commitment<E>>> {
    let mut commitments = BTreeMap::new();
    for (synced, (program, target)) in targets.iter().enumerate() {
        let source = Remote {
            asking: asking.clone(),
            program: program.clone(),
        };
        let commitment = Commitment::sync_from(
            context
                .child("sync")
                .with_attribute("program", program.as_str()),
            &commitment_name(name, program),
            target.clone(),
            source,
        )
        .await
        .map_err(|failure| match failure {
            SyncFailure::Source(error) => error,
            SyncFailure::State(error) => Error::State(error),
        })?;
        commitments.insert(program.clone(), commitment);
        tracing::debug!(
            target: "ducktape::sync",
            event = "join_synced",
            program = program.as_str(),
            synced = synced + 1,
            programs = targets.len(),
            "the join synced a program's commitment"
        );
        release(asking, hold, program);
    }
    Ok(commitments)
}

fn release<E: Context, X: Exchange>(asking: &Asking<E, X>, hold: Option<u64>, program: &ProgramId) {
    let Some(hold) = hold else {
        return;
    };
    let release = Request::Release {
        hold,
        program: program.clone(),
    };
    let exchange = asking.exchange.clone();
    let program = program.clone();
    let sent = asking.context.child("release").spawn(move |_| async move {
        let answered = exchange.exchange(release).await;
        let taken = matches!(answered, Ok(Response::Released));
        if taken {
            return;
        }
        tracing::debug!(
            target: "ducktape::sync",
            event = "join_release_untaken",
            program = program.as_str(),
            hold,
            "the source did not take the release of a program the join synced; its hold keeps it until the join ends"
        );
    });
    drop(sent);
}

fn pruned_past(error: &Error) -> bool {
    matches!(error, Error::Refused(refusal) if refusal.reason == reason::STALE)
}

fn verified<E: Context>(
    context: &mut E,
    network: &[u8],
    node: &Node<E>,
    certificate: &Certificate,
) -> Result<()> {
    let epoch = certificate.round().epoch().get();
    let seated = node
        .epoch_validators(epoch)?
        .ok_or(Error::Unrecorded { epoch })?;
    let validators = validators_of(&seated).ok_or(Error::Certificate { epoch })?;
    let scheme = Scheme::verifier(network, validators);
    let verifies = certificate.verify(context, &scheme, &Sequential);
    if !verifies {
        return Err(Error::Certificate { epoch });
    }
    Ok(())
}

async fn head<E: Context, X: Exchange>(asking: &Asking<E, X>) -> Result<Head> {
    match asking.ask(Request::Head).await? {
        Response::Head(head) => Ok(head),
        Response::Refused(refusal) => Err(Error::Refused(refusal)),
        other => Err(Error::Answer(Box::new(other))),
    }
}

async fn held_head<E: Context, X: Exchange>(
    asking: &Asking<E, X>,
) -> Result<(Head, Option<u64>, Ending)> {
    let held = asking.held().await?;
    match held.head {
        Response::Head(head) => Ok((head, held.hold, held.ending)),
        Response::Refused(refusal) => Err(Error::Refused(refusal)),
        other => Err(Error::Answer(Box::new(other))),
    }
}

struct Joining<'a, E: Context> {
    node: &'a mut Node<E>,
    fetched: &'a mut BTreeSet<BlobId>,
}

impl<E: Context> Install for Joining<'_, E> {
    async fn install(&mut self, id: BlobId, framed: Vec<u8>) -> Result<()> {
        self.node.install(id, &framed)?;
        self.fetched.insert(id);
        Ok(())
    }
}

fn unheld_code<E: Context>(node: &Node<E>) -> Result<Vec<BlobId>> {
    let host = node.host();
    let code: BTreeSet<BlobId> = host
        .programs()?
        .into_values()
        .filter(|code| !host.blobs().has(code))
        .collect();
    Ok(code.into_iter().collect())
}

async fn installed<E: Context, X: Exchange>(
    asking: &Asking<E, X>,
    mut missing: Vec<BlobId>,
    install: &mut impl Install,
) -> Result<Vec<BlobId>> {
    tracing::info!(
        target: "ducktape::sync",
        event = "join_fetching_blobs",
        blobs = missing.len(),
        "the node asks its source for the blobs it lacks, in one stream"
    );
    let mut attempts: u64 = 0;
    let mut progress = Progress {
        blobs: missing.len(),
        installed: 0,
    };
    let mut unserved = Vec::new();
    while !missing.is_empty() {
        let streamed = streamed(&asking.exchange, install, &missing, &mut progress).await?;
        missing.drain(..streamed.answered);
        unserved.extend(streamed.unserved);
        if let Some(error) = streamed.cut {
            asking.again(error, &mut attempts).await?;
        }
    }
    Ok(unserved)
}

struct Progress {
    blobs: usize,
    installed: usize,
}

impl Progress {
    fn installed(&mut self) {
        let percent = |installed: usize| installed * 100 / self.blobs;
        let before = percent(self.installed);
        self.installed += 1;
        let crossed_a_percent = percent(self.installed) != before;
        if !crossed_a_percent {
            return;
        }
        tracing::debug!(
            target: "ducktape::sync",
            event = "join_blobs",
            installed = self.installed,
            blobs = self.blobs,
            "the node installed another hundredth of the blobs it asked its source for"
        );
    }
}

struct Streamed<F> {
    answered: usize,
    unserved: Vec<BlobId>,
    cut: Option<F>,
}

async fn streamed<X: Exchange>(
    exchange: &X,
    install: &mut impl Install,
    missing: &[BlobId],
    progress: &mut Progress,
) -> Result<Streamed<X::Error>> {
    let mut answers = std::pin::pin!(exchange.stream_blobs(missing.to_vec()));
    let mut streamed = Streamed {
        answered: 0,
        unserved: Vec::new(),
        cut: None,
    };
    while let Some(answer) = answers.next().await {
        let framed = match answer {
            Ok(framed) => framed,
            Err(error) => {
                streamed.cut = Some(error);
                return Ok(streamed);
            }
        };
        let Some(&id) = missing.get(streamed.answered) else {
            return Err(Error::Blobs {
                asked: missing.len(),
                answered: streamed.answered + 1,
            });
        };
        streamed.answered += 1;
        let Some(framed) = framed else {
            streamed.unserved.push(id);
            continue;
        };
        install.install(id, framed).await?;
        progress.installed();
    }
    let answered_every_blob = streamed.answered == missing.len();
    if !answered_every_blob {
        return Err(Error::Blobs {
            asked: missing.len(),
            answered: streamed.answered,
        });
    }
    Ok(streamed)
}

struct Asking<E, X> {
    context: E,
    exchange: X,
    pause: Duration,
}

impl<E: Context, X: Exchange> Clone for Asking<E, X> {
    fn clone(&self) -> Self {
        Asking {
            context: self.context.child("ask"),
            exchange: self.exchange.clone(),
            pause: self.pause,
        }
    }
}

impl<E: Context, X: Exchange> Asking<E, X> {
    async fn ask(&self, request: Request) -> Result<Response> {
        let mut attempts: u64 = 0;
        loop {
            let error = match self.exchange.exchange(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            self.again(error, &mut attempts).await?;
        }
    }

    async fn held(&self) -> Result<Held> {
        let mut attempts: u64 = 0;
        loop {
            let error = match self.exchange.hold().await {
                Ok(held) => return Ok(held),
                Err(error) => error,
            };
            self.again(error, &mut attempts).await?;
        }
    }

    async fn again(&self, error: X::Error, attempts: &mut u64) -> Result<()> {
        let Failure::Transient(reason) = error.failure() else {
            return Err(Error::Exchange(Box::new(error)));
        };
        self.waited(reason, attempts).await;
        Ok(())
    }

    async fn waited(&self, reason: &'static str, attempts: &mut u64) {
        *attempts += 1;
        let worth_a_line = *attempts == 1 || attempts.is_multiple_of(ATTEMPTS_PER_LINE);
        if worth_a_line {
            tracing::warn!(
                target: "ducktape::sync",
                event = "join_unanswered",
                reason,
                attempts = *attempts,
                "the source did not answer the join; it asks again"
            );
        }
        self.context.sleep(self.pause).await;
    }
}
