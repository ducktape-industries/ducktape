// The node's pool task: on a thread of its own it owns the mempool, answers each submit over a
// oneshot against the newest view and what it already holds, rechecks what it holds after each
// block, and builds the blocks consensus proposes. Nothing it does waits on the writer.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use abi::Refusal;
use host::{Submission, Tip};
use node::{Block, Mempool, Preconfirm, Source, Taken};
use tokio::sync::{mpsc, oneshot, watch};

use crate::relay::Relay;
use crate::view::{Current, Speculative, View};
use crate::{Context, Error, Result};

const TARGET: &str = "ducktape::pool";

type Answer = Result<std::result::Result<Taken, Refusal>>;

pub(crate) struct Submit {
    frame: Vec<u8>,
    submission: Submission,
    source: Source,
    preconfirm: Preconfirm,
    reply: oneshot::Sender<Answer>,
}

pub(crate) struct Receipt {
    pub(crate) bundle: u64,
    pub(crate) posted: Option<Vec<u8>>,
    pub(crate) fresh: Vec<u8>,
    pub(crate) reseated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reposted {
    Pending,
    Retaken,
    Refused,
    Unheld,
}

impl Reposted {
    fn of(taken: &Answer) -> Reposted {
        match taken {
            Ok(Ok(Taken::Held(_))) => Reposted::Pending,
            Ok(Ok(Taken::New(_))) => Reposted::Retaken,
            Ok(Err(refusal)) if refusal.reason == abi::reason::DUPLICATE => Reposted::Refused,
            Ok(Err(_)) | Err(_) => Reposted::Unheld,
        }
    }

    fn wants_fresh(self, reseated: bool) -> bool {
        match self {
            Reposted::Pending | Reposted::Retaken => false,
            Reposted::Refused => reseated,
            Reposted::Unheld => true,
        }
    }
}

pub(crate) struct Build {
    parent: Tip,
    unapplied: Vec<Arc<Block>>,
    time: u64,
    reply: oneshot::Sender<Block>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Offer {
    Due,
    Every,
}

pub(crate) enum Event<E: Context> {
    Build(Build),
    Receipt(Receipt),
    Viewed(Arc<View<E>>),
    Withdrawn,
    Replay,
    Submit(Submit),
    Offer(Offer),
    Close(oneshot::Sender<()>),
    Unasked,
}

enum Flow {
    Pooling,
    Closing(oneshot::Sender<()>),
    Closed,
}

pub(crate) struct Pooling<E: Context> {
    inbox: mpsc::UnboundedSender<Event<E>>,
    builds: mpsc::UnboundedSender<Build>,
    receipts: mpsc::UnboundedSender<Receipt>,
    applied: Arc<AtomicU64>,
}

impl<E: Context> Clone for Pooling<E> {
    fn clone(&self) -> Pooling<E> {
        Pooling {
            inbox: self.inbox.clone(),
            builds: self.builds.clone(),
            receipts: self.receipts.clone(),
            applied: self.applied.clone(),
        }
    }
}

impl<E: Context> Pooling<E> {
    pub(crate) async fn submit(
        &self,
        frame: Vec<u8>,
        submission: Submission,
        source: Source,
        preconfirm: Preconfirm,
    ) -> Answer {
        let (reply, answer) = oneshot::channel();
        let submit = Submit {
            frame,
            submission,
            source,
            preconfirm,
            reply,
        };
        self.inbox
            .send(Event::Submit(submit))
            .map_err(|_| Error::Stopped)?;
        answer.await.map_err(|_| Error::Stopped)?
    }

    pub(crate) fn offer(&self, offer: Offer) {
        let _ = self.inbox.send(Event::Offer(offer));
    }

    pub(crate) fn receipt(&self, receipt: Receipt) {
        let _ = self.receipts.send(receipt);
    }

    pub(crate) fn applied(&self) -> u64 {
        self.applied.load(Ordering::Acquire)
    }

    pub(crate) async fn build(
        &self,
        parent: Tip,
        unapplied: Vec<Arc<Block>>,
        time: u64,
    ) -> Option<Block> {
        let (reply, built) = oneshot::channel();
        let build = Build {
            parent,
            unapplied,
            time,
            reply,
        };
        self.builds.send(build).ok()?;
        built.await.ok()
    }

    pub(crate) async fn close(&self) {
        let (reply, closed) = oneshot::channel();
        if self.inbox.send(Event::Close(reply)).is_ok() {
            let _ = closed.await;
        }
    }
}

struct Pool<E: Context> {
    mempool: Mempool<E>,
    base: Arc<View<E>>,
    published: Arc<Current<E>>,
    applied: Arc<AtomicU64>,
    relay: Arc<Relay<E>>,
    network: Vec<u8>,
    posts: blob::service::Posts,
}

struct Wakes<E: Context> {
    builds: mpsc::UnboundedReceiver<Build>,
    receipts: mpsc::UnboundedReceiver<Receipt>,
    tips: watch::Receiver<u64>,
    inbox: mpsc::UnboundedReceiver<Event<E>>,
}

pub(crate) const THREAD: &str = "pool";

pub(crate) fn start<E: Context>(
    context: E,
    network: Vec<u8>,
    base: Arc<View<E>>,
    published: Arc<Current<E>>,
    relay: Arc<Relay<E>>,
    posts: blob::service::Posts,
    tips: watch::Receiver<u64>,
) -> Result<Pooling<E>> {
    let mempool = Mempool::new(context, network.clone(), base.snapshot())?;
    let applied = Arc::new(AtomicU64::new(mempool.applied().height));
    published
        .speculative
        .store(Some(Arc::new(speculative(&base, &mempool))));
    let (inbox, asked) = mpsc::unbounded_channel();
    let (builds, building) = mpsc::unbounded_channel();
    let (receipts, receipting) = mpsc::unbounded_channel();
    let pooling = Pooling {
        inbox,
        builds,
        receipts,
        applied: applied.clone(),
    };
    let pool = Pool {
        mempool,
        base,
        published,
        applied,
        relay,
        network,
        posts,
    };
    let wakes = Wakes {
        builds: building,
        receipts: receipting,
        tips,
        inbox: asked,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name(THREAD.into())
        .spawn(move || runtime.block_on(pooled(pool, wakes)))?;
    Ok(pooling)
}

async fn pooled<E: Context>(mut pool: Pool<E>, mut wakes: Wakes<E>) {
    loop {
        let event = next(&pool.published, pool.mempool.replaying(), &mut wakes).await;
        let flow = match event {
            Event::Build(build) => built(&mut pool, build),
            Event::Receipt(receipt) => receipted(&mut pool, receipt).await,
            Event::Viewed(view) => viewed(&mut pool, view),
            Event::Withdrawn => withdrawn(),
            Event::Replay => replayed(&mut pool).await,
            Event::Submit(submit) => submitted(&mut pool, submit).await,
            Event::Offer(offer) => offered(&mut pool, offer),
            Event::Close(closer) => Flow::Closing(closer),
            Event::Unasked => Flow::Closed,
        };
        match flow {
            Flow::Pooling => {}
            Flow::Closing(closer) => return close(pool, closer),
            Flow::Closed => return,
        }
    }
}

async fn next<E: Context>(
    published: &Current<E>,
    replaying: bool,
    wakes: &mut Wakes<E>,
) -> Event<E> {
    tokio::select! {
        biased;
        build = wakes.builds.recv() => build.map_or(Event::Unasked, Event::Build),
        changed = wakes.tips.changed() => match (changed, published.views.load_full()) {
            (Ok(()), Some(view)) => Event::Viewed(view),
            (Ok(()), None) => Event::Withdrawn,
            (Err(_), _) => Event::Unasked,
        },
        () = std::future::ready(()), if replaying => Event::Replay,
        receipt = wakes.receipts.recv() => receipt.map_or(Event::Unasked, Event::Receipt),
        asked = wakes.inbox.recv() => asked.unwrap_or(Event::Unasked),
    }
}

fn built<E: Context>(pool: &mut Pool<E>, build: Build) -> Flow {
    let Build {
        parent,
        unapplied,
        time,
        reply,
    } = build;
    let block = pool
        .mempool
        .build(parent, unapplied.iter().map(AsRef::as_ref), time);
    let _ = reply.send(block);
    Flow::Pooling
}

fn viewed<E: Context>(pool: &mut Pool<E>, view: Arc<View<E>>) -> Flow {
    let same_height = view.tip().height == pool.base.tip().height;
    if same_height {
        return Flow::Pooling;
    }
    let stale = match pool.mempool.rebase(view.snapshot()) {
        Ok(stale) => stale,
        Err(error) => {
            tracing::warn!(
                target: TARGET,
                event = "pool_unrebased",
                height = view.tip().height,
                %error,
                reason = "rebase_failed",
                "the pool could not recheck what it holds against a new block"
            );
            return Flow::Pooling;
        }
    };
    tracing::debug!(
        target: TARGET,
        height = view.tip().height,
        dropped = stale.len(),
        held = pool.mempool.len(),
        "the pool rechecked what it holds against a new block"
    );
    pool.base = view;
    pool.applied
        .store(pool.mempool.applied().height, Ordering::Release);
    publish(pool);
    Flow::Pooling
}

fn withdrawn() -> Flow {
    Flow::Pooling
}

async fn replayed<E: Context>(pool: &mut Pool<E>) -> Flow {
    if let Err(error) = pool.mempool.replay().await {
        tracing::warn!(
            target: TARGET,
            event = "pool_unreplayed",
            %error,
            reason = "replay_failed",
            "the pool could not predict a held frame again over a new block"
        );
    }
    let replayed = !pool.mempool.replaying();
    if replayed {
        publish(pool);
    }
    Flow::Pooling
}

async fn submitted<E: Context>(pool: &mut Pool<E>, submit: Submit) -> Flow {
    let Submit {
        frame,
        submission,
        source,
        preconfirm,
        reply,
    } = submit;
    let taken = take(pool, frame, submission, source, preconfirm).await;
    let _ = reply.send(taken);
    Flow::Pooling
}

async fn take<E: Context>(
    pool: &mut Pool<E>,
    frame: Vec<u8>,
    submission: Submission,
    source: Source,
    preconfirm: Preconfirm,
) -> Answer {
    let taken = pool
        .mempool
        .take(frame, submission, source, preconfirm)
        .await
        .map_err(Error::from);
    let predicted = matches!(taken, Ok(Ok(Taken::New(Some(_)))));
    if predicted {
        publish(pool);
    }
    taken
}

async fn own<E: Context>(pool: &mut Pool<E>, frame: Vec<u8>) -> Answer {
    let submission = match node::admissible(&frame, &pool.network) {
        Ok(submission) => submission,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let taken = take(pool, frame, submission, Source::Client, Preconfirm::Off).await;
    if let Err(error) = &taken {
        tracing::warn!(
            target: TARGET,
            event = "receipt_untaken",
            %error,
            reason = "take_failed",
            "the pool could not take the node's own receipt"
        );
    }
    taken
}

async fn receipted<E: Context>(pool: &mut Pool<E>, receipt: Receipt) -> Flow {
    let Receipt {
        bundle,
        posted,
        fresh,
        reseated,
    } = receipt;
    let reposted = match posted {
        Some(frame) => {
            let reposted = Reposted::of(&own(pool, frame.clone()).await);
            if reposted == Reposted::Retaken {
                pool.relay.send([frame.as_slice()]);
            }
            reposted
        }
        None => Reposted::Unheld,
    };
    if !reposted.wants_fresh(reseated) {
        return Flow::Pooling;
    }
    match own(pool, fresh.clone()).await {
        Ok(Ok(Taken::New(_))) => {
            pool.posts.posted(bundle, fresh.clone());
            pool.relay.send([fresh.as_slice()]);
        }
        Ok(Ok(Taken::Held(_))) | Err(_) => {}
        Ok(Err(refusal)) => tracing::debug!(
            target: "ducktape::blob",
            bundle,
            reason = refusal.reason,
            "the node did not take its own receipt"
        ),
    }
    Flow::Pooling
}

fn offered<E: Context>(pool: &mut Pool<E>, offer: Offer) -> Flow {
    let frames = match offer {
        Offer::Due => pool.mempool.reoffer(),
        Offer::Every => pool.mempool.reoffer_all(),
    };
    pool.relay.send(frames.iter().map(Vec::as_slice));
    Flow::Pooling
}

fn close<E: Context>(pool: Pool<E>, closer: oneshot::Sender<()>) {
    pool.published.speculative.store(None);
    drop(pool);
    let _ = closer.send(());
}

fn publish<E: Context>(pool: &Pool<E>) {
    let published = speculative(&pool.base, &pool.mempool);
    pool.published.speculative.store(Some(Arc::new(published)));
}

fn speculative<E: Context>(base: &Arc<View<E>>, mempool: &Mempool<E>) -> Speculative<E> {
    Speculative::over(base.clone(), mempool.speculative().clone())
}

#[cfg(test)]
mod tests {
    use abi::Refusal;
    use commonware_runtime::tokio::Context;
    use tokio::sync::{mpsc, watch};

    use super::{Answer, Event, Offer, Receipt, Reposted, Wakes, next};
    use crate::view::Current;

    #[test]
    fn a_receipt_is_signed_again_only_when_its_posted_one_is_gone() {
        let refused = |reason| -> Answer { Ok(Err(Refusal::new(reason, "refused"))) };
        assert_eq!(
            Reposted::of(&refused(abi::reason::DUPLICATE)),
            Reposted::Refused
        );
        assert_eq!(
            Reposted::of(&refused(abi::reason::INVALID_INPUT)),
            Reposted::Unheld
        );
        assert_eq!(Reposted::of(&Err(crate::Error::Stopped)), Reposted::Unheld);
        assert!(!Reposted::Pending.wants_fresh(true));
        assert!(!Reposted::Retaken.wants_fresh(true));
        assert!(!Reposted::Refused.wants_fresh(false));
        assert!(Reposted::Refused.wants_fresh(true));
        assert!(Reposted::Unheld.wants_fresh(false));
    }

    #[tokio::test]
    async fn the_writers_receipts_follow_their_block_and_go_before_every_client_submit() {
        let (inbox, asked) = mpsc::unbounded_channel();
        let (_builds, building) = mpsc::unbounded_channel();
        let (receipts, receipting) = mpsc::unbounded_channel();
        let (tips, tipped) = watch::channel(0);
        let mut wakes = Wakes::<Context> {
            builds: building,
            receipts: receipting,
            tips: tipped,
            inbox: asked,
        };
        let published = Current::<Context>::new();
        inbox.send(Event::Offer(Offer::Due)).unwrap();
        inbox.send(Event::Offer(Offer::Every)).unwrap();
        tips.send_replace(1);
        receipts
            .send(Receipt {
                bundle: 0,
                posted: None,
                fresh: Vec::new(),
                reseated: false,
            })
            .unwrap();
        let event = next(&published, false, &mut wakes).await;
        assert!(matches!(event, Event::Withdrawn));
        let event = next(&published, false, &mut wakes).await;
        assert!(matches!(event, Event::Receipt(_)));
        let event = next(&published, false, &mut wakes).await;
        assert!(matches!(event, Event::Offer(Offer::Due)));
    }
}
