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
    applied: Arc<AtomicU64>,
}

impl<E: Context> Clone for Pooling<E> {
    fn clone(&self) -> Pooling<E> {
        Pooling {
            inbox: self.inbox.clone(),
            builds: self.builds.clone(),
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
}

struct Wakes<E: Context> {
    builds: mpsc::UnboundedReceiver<Build>,
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
    tips: watch::Receiver<u64>,
) -> Result<Pooling<E>> {
    let mempool = Mempool::new(context, network, base.snapshot())?;
    let applied = Arc::new(AtomicU64::new(mempool.applied().height));
    published
        .speculative
        .store(Some(Arc::new(speculative(&base, &mempool))));
    let (inbox, asked) = mpsc::unbounded_channel();
    let (builds, building) = mpsc::unbounded_channel();
    let pooling = Pooling {
        inbox,
        builds,
        applied: applied.clone(),
    };
    let pool = Pool {
        mempool,
        base,
        published,
        applied,
        relay,
    };
    let wakes = Wakes {
        builds: building,
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
        let event = next(&pool, &mut wakes).await;
        let flow = match event {
            Event::Build(build) => built(&pool, build),
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

async fn next<E: Context>(pool: &Pool<E>, wakes: &mut Wakes<E>) -> Event<E> {
    let replaying = pool.mempool.replaying();
    tokio::select! {
        biased;
        build = wakes.builds.recv() => build.map_or(Event::Unasked, Event::Build),
        changed = wakes.tips.changed() => match (changed, pool.published.views.load_full()) {
            (Ok(()), Some(view)) => Event::Viewed(view),
            (Ok(()), None) => Event::Withdrawn,
            (Err(_), _) => Event::Unasked,
        },
        () = std::future::ready(()), if replaying => Event::Replay,
        asked = wakes.inbox.recv() => asked.unwrap_or(Event::Unasked),
    }
}

fn built<E: Context>(pool: &Pool<E>, build: Build) -> Flow {
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
    let taken = pool
        .mempool
        .take(frame, submission, source, preconfirm)
        .await
        .map_err(Error::from);
    let predicted = matches!(taken, Ok(Ok(Taken::New(Some(_)))));
    if predicted {
        publish(pool);
    }
    let _ = reply.send(taken);
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
