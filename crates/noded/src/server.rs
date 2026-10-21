use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use abi::Receipt;
use abi::{BlobId, ProgramId, Refusal, reason, role};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{ConnectInfo, FromRequest, MatchedPath, Path, RawQuery, Request};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::ListenerExt as _;
use borsh::BorshSerialize;
use futures::pin_mut;
use futures::stream::SplitSink;
use futures::{SinkExt as _, StreamExt as _};
use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use node::{BLOCK_BYTES, Preconfirm, Taken};
use statesync::Request as SyncRequest;
use tower_http::cors::{Any, CorsLayer};

use crate::changes::{self, Fanout, Published, Subscription};
use crate::ctx::{Ctx, EgressSigner};
use crate::wire::{
    Admin, BlockAsk, Blocks, EgressPlan, EgressSet, Finalized, Get, Located, Network, Page, Query,
    Range, Submit, Submitted, TxAsk, route,
};
use crate::{Context, Daemon, cores, offload};

const SUBMIT_BYTES: usize = BLOCK_BYTES;
const SUBMIT_BODY_BYTES: usize = SUBMIT_BYTES + size_of::<u32>() + size_of::<bool>();

pub(crate) struct Rpc(Option<tokio::runtime::Runtime>);

impl Rpc {
    pub(crate) fn start<E: Context>(
        daemon: Arc<Daemon<E>>,
        listener: std::net::TcpListener,
        fanout: Fanout<E>,
        answers: tokio::sync::mpsc::UnboundedReceiver<Answered>,
        queries: tokio::sync::mpsc::UnboundedReceiver<crate::census::Queried>,
        rpc_cpus: Option<Vec<usize>>,
        blocking_threads: Option<std::num::NonZeroUsize>,
    ) -> crate::Result<Rpc> {
        let pinned = rpc_cpus.is_some();
        let cpus = match rpc_cpus {
            Some(cpus) => cpus,
            None => cores::usable()?,
        };
        let threads = cpus.len();
        tracing::info!(target: "ducktape::http", event = "rpc_cores", rpc = threads, pinned, "the rpc runtime runs beside the block threads");
        let cpus = Arc::new(cpus);
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder
            .worker_threads(threads)
            .thread_name("rpc")
            .on_thread_start(move || yielding(&cpus))
            .enable_all();
        if let Some(blocking) = blocking_threads {
            builder.max_blocking_threads(blocking.get());
        }
        let runtime = builder.build()?;
        let block_time = Duration::from_millis(daemon.network.cadence.block_time_ms());
        let router = router(daemon.clone());
        runtime.spawn(slow_lines(
            block_time,
            answers,
            daemon.tips.subscribe(),
            daemon.holds_ended.clone(),
        ));
        runtime.spawn(changes::fan(fanout));
        runtime.spawn(crate::census::census(
            daemon.network.epoch_length,
            queries,
            daemon.tips.subscribe(),
        ));
        for _ in 0..threads {
            let door = listener.try_clone()?;
            runtime.spawn(accepting(door, router.clone(), daemon.shutdown.subscribe()));
        }
        Ok(Rpc(Some(runtime)))
    }

    pub(crate) fn handle(&self) -> Option<tokio::runtime::Handle> {
        self.0.as_ref().map(|runtime| runtime.handle().clone())
    }
}

async fn accepting(
    door: std::net::TcpListener,
    router: Router,
    mut down: tokio::sync::watch::Receiver<bool>,
) {
    let listener = match crate::transport::served(door) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(target: "ducktape::http", %error, "the http server stopped");
            return;
        }
    };
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(target: "ducktape::http", %error, "a connection keeps nagle");
        }
    });
    let served = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = down.wait_for(|down| *down).await;
    })
    .await;
    if let Err(error) = served {
        tracing::error!(target: "ducktape::http", %error, "the http server stopped");
    }
}

fn yielding(cpus: &[usize]) {
    lowest_priority();
    if let Err(error) = cores::pin(cpus) {
        tracing::debug!(target: "ducktape::http", %error, "an rpc thread runs on every cpu");
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn lowest_priority() {
    const LOWEST: i32 = 19;
    let thread = rustix::thread::gettid();
    if let Err(error) = rustix::process::setpriority_process(Some(thread), LOWEST) {
        tracing::debug!(target: "ducktape::http", %error, "an rpc thread keeps its priority");
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn lowest_priority() {}

impl Drop for Rpc {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

pub(crate) struct Answered {
    route: String,
    spent: Duration,
}

#[derive(Default)]
struct Slowed {
    count: u64,
    max_ms: u64,
    slowest: String,
}

impl Slowed {
    fn observe(&mut self, answered: Answered, slow: Duration) {
        if answered.spent <= slow {
            return;
        }
        let elapsed_ms = answered.spent.as_millis() as u64;
        self.count += 1;
        if elapsed_ms >= self.max_ms {
            self.max_ms = elapsed_ms;
            self.slowest = answered.route;
        }
    }
}

enum Heard {
    Tip,
    Answered(Answered),
    Ended,
}

fn router<E: Context>(daemon: Arc<Daemon<E>>) -> Router {
    Router::new()
        .route(route::STATUS, get(status::<E>))
        .route(route::NETWORK, get(network::<E>))
        .route(route::HALO_SELF, get(halo_self::<E>))
        .route(route::GET, post(get_value::<E>))
        .route(route::SCAN, post(scan::<E>))
        .route(route::BLOB_GET, post(blob_get::<E>))
        .route(route::BLOBS, post(blobs::<E>))
        .route(route::BLOBS_ZSTD, post(blobs_zstd::<E>))
        .route(route::BLOBS_HELD, post(blobs_held::<E>))
        .route(route::BLOB_MISSING, get(blob_missing::<E>))
        .route(route::PROGRAMS, get(programs::<E>))
        .route(route::BLOCKS, post(blocks::<E>))
        .route(route::BLOCK, post(block::<E>))
        .route(route::TX, post(tx::<E>))
        .route(route::ACTIVITY, post(activity::<E>))
        .route(route::LOGS, get(logs::<E>))
        .route(route::ADMIN, post(admin::<E>))
        .route(route::EGRESS_SET, post(egress_set::<E>))
        .route(route::SYNC, post(sync::<E>))
        .route(route::SYNC_HOLD, post(sync_hold::<E>))
        .route(route::SNAPSHOT, post(snapshot::<E>))
        .route(route::METRICS, get(metrics::<E>))
        .route(live_wire::PORT_ROUTE, get(live_port::<E>))
        .route(route::QUERY, post(query::<E>))
        .route(route::SUBMIT, post(submit::<E>))
        .route(route::BLOB_PUT, post(blob_put::<E>))
        .route(
            &format!("{}/{{*program}}", route::CHANGES),
            get(changes::<E>),
        )
        .route(
            &format!("{}/{{*program}}", route::WATCH),
            get(watching::<E>),
        )
        .route(route::HEADS, get(heads::<E>))
        .route(route::P2P, get(p2p::<E>))
        .route(live_wire::ROUTE, get(live_socket::<E>))
        .route(halo::RELAY_ROUTE, get(halo_relay::<E>))
        .route(halo::SEEN_ROUTE, get(seen))
        .route(route::FORWARD, get(forward::<E>))
        .layer(axum::middleware::from_fn_with_state(
            daemon.clone(),
            timed::<E>,
        ))
        .with_state(daemon)
        .layer(cors())
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}

async fn timed<E: Context>(
    ctx: Ctx<E>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());
    let method = request.method().clone();
    let started = Instant::now();
    let response = next.run(request).await;
    let spent = started.elapsed();
    let route = route.as_deref().unwrap_or("unrouted");
    let status = response.status().as_u16();
    let elapsed_ms = spent.as_millis() as u64;
    tracing::debug!(
        target: "ducktape::http",
        route,
        %method,
        status,
        elapsed_ms,
        "a request answered its head"
    );
    ctx.answered(Answered {
        route: route.to_owned(),
        spent,
    });
    response
}

async fn slow_lines(
    slow: Duration,
    mut answers: tokio::sync::mpsc::UnboundedReceiver<Answered>,
    mut tips: tokio::sync::watch::Receiver<u64>,
    holds_ended: Arc<AtomicU64>,
) {
    let mut window = Slowed::default();
    loop {
        let heard = tokio::select! {
            biased;
            changed = tips.changed() => match changed {
                Ok(()) => Heard::Tip,
                Err(_) => Heard::Ended,
            },
            Some(answered) = answers.recv() => Heard::Answered(answered),
        };
        match heard {
            Heard::Tip => window = tipped(window, slow, &mut answers, &mut tips, &holds_ended),
            Heard::Answered(answered) => window.observe(answered, slow),
            Heard::Ended => return,
        }
    }
}

fn tipped(
    window: Slowed,
    slow: Duration,
    answers: &mut tokio::sync::mpsc::UnboundedReceiver<Answered>,
    tips: &mut tokio::sync::watch::Receiver<u64>,
    holds_ended: &AtomicU64,
) -> Slowed {
    let height = *tips.borrow_and_update();
    let window = closed(window, slow, answers, height);
    holds_closed(holds_ended, height);
    window
}

fn closed(
    mut window: Slowed,
    slow: Duration,
    answers: &mut tokio::sync::mpsc::UnboundedReceiver<Answered>,
    height: u64,
) -> Slowed {
    while let Ok(answered) = answers.try_recv() {
        window.observe(answered, slow);
    }
    if window.count > 0 {
        let slowed = window;
        tracing::warn!(
            target: "ducktape::http",
            event = "request_slow",
            height,
            count = slowed.count,
            max_ms = slowed.max_ms,
            slowest = slowed.slowest,
            block_time_ms = slow.as_millis() as u64,
            reason = "block_time_exceeded",
            "requests took longer than a block time to answer their heads since the block before"
        );
    }
    Slowed::default()
}

fn holds_closed(ended: &AtomicU64, height: u64) {
    let count = ended.swap(0, Ordering::Relaxed);
    if count > 0 {
        tracing::info!(
            target: "ducktape::sync",
            event = "sync_holds_ended",
            height,
            count,
            "joins' holds on this node's state ended since the block before"
        );
    }
}

struct Whole(Bytes);

impl<S: Send + Sync> FromRequest<S> for Whole {
    type Rejection = Reply<()>;

    async fn from_request(request: Request, _: &S) -> Result<Self, Reply<()>> {
        match request.into_body().collect().await {
            Ok(body) => Ok(Whole(body.to_bytes())),
            Err(error) => Err(cut(error)),
        }
    }
}

struct Capped<const MAX: usize>(Bytes);

impl<S: Send + Sync, const MAX: usize> FromRequest<S> for Capped<MAX> {
    type Rejection = Reply<()>;

    async fn from_request(request: Request, _: &S) -> Result<Self, Reply<()>> {
        let read = Limited::new(request.into_body(), MAX).collect().await;
        match read {
            Ok(body) => Ok(Capped(body.to_bytes())),
            Err(error) if error.is::<LengthLimitError>() => Err(Reply::Refused(Refusal::new(
                reason::CAPACITY,
                format!("the request is over the {MAX} bytes this route reads"),
            ))),
            Err(error) => Err(cut(error)),
        }
    }
}

fn cut(error: impl std::fmt::Display) -> Reply<()> {
    Reply::Refused(Refusal::new(
        reason::INVALID_INPUT,
        format!("the request body did not arrive whole: {error}"),
    ))
}

pub enum Reply<T> {
    Answered(T),
    Refused(Refusal),
    Failed(String),
}

impl<T: BorshSerialize> IntoResponse for Reply<T> {
    fn into_response(self) -> Response {
        match self {
            Reply::Answered(value) => (StatusCode::OK, abi::encode(&value)).into_response(),
            Reply::Refused(refusal) => {
                (StatusCode::BAD_REQUEST, abi::encode(&refusal)).into_response()
            }
            Reply::Failed(sentence) => {
                (StatusCode::INTERNAL_SERVER_ERROR, sentence).into_response()
            }
        }
    }
}

fn failed<T>(error: impl ToString) -> Reply<T> {
    Reply::Failed(error.to_string())
}

fn answered<T, E: ToString>(read: crate::Result<Result<T, E>>) -> Reply<T> {
    match read {
        Ok(Ok(value)) => Reply::Answered(value),
        Ok(Err(error)) => failed(error),
        Err(error) => failed(error),
    }
}

struct Encoded(Bytes);

impl IntoResponse for Encoded {
    fn into_response(self) -> Response {
        (StatusCode::OK, self.0).into_response()
    }
}

async fn status<E: Context>(ctx: Ctx<E>) -> Encoded {
    Encoded(ctx.view().status())
}

async fn network<E: Context>(ctx: Ctx<E>) -> Reply<Network> {
    match ctx.network().await {
        Ok(network) => Reply::Answered(network),
        Err(error) => failed(error),
    }
}

async fn submit<E: Context>(
    ctx: Ctx<E>,
    Capped(body): Capped<SUBMIT_BODY_BYTES>,
) -> Reply<Submitted> {
    let Submit { frame, preconfirm } = match abi::decode(&body) {
        Ok(submit) => submit,
        Err(refusal) => return Reply::Refused(refusal),
    };
    taken(ctx.submit(frame, preconfirm).await)
}

fn taken(taken: crate::Result<Result<Taken, Refusal>>) -> Reply<Submitted> {
    match taken {
        Ok(Ok(taken)) => Reply::Answered(submitted(taken.into_prediction())),
        Ok(Err(refusal)) => Reply::Refused(refusal),
        Err(error) => failed(error),
    }
}

fn submitted(prediction: Option<Receipt>) -> Submitted {
    match prediction {
        Some(receipt) => Submitted::Preconfirmed(receipt),
        None => Submitted::Pooled,
    }
}

async fn query<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<Vec<u8>> {
    let query: Query = match abi::decode(&body) {
        Ok(query) => query,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let reading = match ctx.reading(query.layer) {
        Ok(reading) => reading,
        Err(error) => return failed(error),
    };
    let network = ctx.network_id();
    let time = ctx.now();
    let newest = ctx.view().clone();
    let asked = offload(move || {
        let source = reading.source();
        let anchors = newest.source();
        tokio::runtime::Handle::current().block_on(crate::query::answer(
            &source, &anchors, query, &network, time,
        ))
    })
    .await;
    match asked {
        Ok(Ok(Ok((program, queried)))) => counted(&ctx, program, queried),
        Ok(Ok(Err(refusal))) => Reply::Refused(refusal),
        Ok(Err(error)) => failed(error),
        Err(error) => failed(error),
    }
}

fn counted<E: Context>(ctx: &Ctx<E>, program: ProgramId, queried: host::Queried) -> Reply<Vec<u8>> {
    tracing::debug!(
        target: "ducktape::query",
        %program,
        fuel = queried.fuel,
        ran_out = queried.ran_out,
        "a query ran"
    );
    ctx.queried(crate::census::Queried {
        program,
        fuel: queried.fuel,
        ran_out: queried.ran_out,
    });
    match queried.answer {
        Ok(bytes) => Reply::Answered(bytes),
        Err(refusal) => Reply::Refused(refusal),
    }
}

async fn get_value<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<Option<Vec<u8>>> {
    let get: Get = match abi::decode(&body) {
        Ok(get) => get,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let reading = match ctx.reading(get.layer) {
        Ok(reading) => reading,
        Err(error) => return failed(error),
    };
    answered(
        offload(move || {
            let source = reading.source();
            match source.place_of(&get.program) {
                Some(place) => source.view(get.layer).get(&place, &get.key),
                None => Ok(None),
            }
        })
        .await,
    )
}

async fn scan<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<Vec<abi::Entry>> {
    let range: Range = match abi::decode(&body) {
        Ok(range) => range,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let reading = match ctx.reading(range.layer) {
        Ok(reading) => reading,
        Err(error) => return failed(error),
    };
    answered(
        offload(move || {
            let source = reading.source();
            match source.place_of(&range.program) {
                Some(place) => source.view(range.layer).scan(&place, &range.scan),
                None => Ok(Vec::new()),
            }
        })
        .await,
    )
}

async fn blob_get<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<Option<Vec<u8>>> {
    let id: BlobId = match abi::decode(&body) {
        Ok(id) => id,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match served(&ctx, id).await {
        Ok(Read::Framed(framed)) => Reply::Answered(Some(framed)),
        Ok(Read::Unnamed) => Reply::Answered(None),
        Ok(Read::Fetching) => Reply::Refused(Refusal::new(
            reason::NOT_FOUND,
            format!(
                "this node does not hold blob {} yet; no member gave it, and the node keeps asking",
                abi::hex(id.digest())
            ),
        )),
        Err(error) => failed(error),
    }
}

async fn blobs<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Response {
    let ids: Vec<BlobId> = match abi::decode(&body) {
        Ok(ids) => ids,
        Err(refusal) => return Reply::<()>::Refused(refusal).into_response(),
    };
    let answer = pieces(ctx, ids).flat_map(futures::stream::iter);
    (StatusCode::OK, Body::from_stream(answer)).into_response()
}

fn pieces<E: Context>(
    ctx: Ctx<E>,
    ids: Vec<BlobId>,
) -> impl futures::Stream<Item = Vec<crate::Result<Bytes>>> + Send + 'static {
    let count = Bytes::copy_from_slice(&(ids.len() as u32).to_le_bytes());
    let reads = futures::stream::iter(ids).then(move |id| {
        let ctx = ctx.clone();
        async move { encoded(served(&ctx, id).await.map(Read::framed)) }
    });
    futures::stream::once(async move { vec![Ok(count)] }).chain(reads)
}

type Encoder = zstd::stream::raw::Encoder<'static>;

async fn blobs_zstd<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Response {
    let ids: Vec<BlobId> = match abi::decode(&body) {
        Ok(ids) => ids,
        Err(refusal) => return Reply::<()>::Refused(refusal).into_response(),
    };
    let encoder = match Encoder::new(zstd::DEFAULT_COMPRESSION_LEVEL) {
        Ok(encoder) => encoder,
        Err(error) => return failed::<()>(error).into_response(),
    };
    let mut pieces = pieces(ctx, ids).boxed();
    let first = pieces.next().await;
    let answer = futures::stream::unfold(Some((pieces, encoder, first)), |state| async move {
        let (mut pieces, encoder, held) = state?;
        let Some(piece) = held else {
            let last = settled(offload(move || finished(encoder)).await);
            return Some((last.map(Bytes::from), None));
        };
        let plain = match piece.into_iter().collect::<crate::Result<Vec<Bytes>>>() {
            Ok(plain) => plain,
            Err(error) => return Some((Err(error), None)),
        };
        let (done, next) =
            futures::join!(offload(move || compressed(encoder, &plain)), pieces.next());
        match settled(done) {
            Ok((encoder, out)) => Some((Ok(Bytes::from(out)), Some((pieces, encoder, next)))),
            Err(error) => Some((Err(error), None)),
        }
    });
    (StatusCode::OK, Body::from_stream(answer)).into_response()
}

fn settled<T>(done: crate::Result<std::io::Result<T>>) -> crate::Result<T> {
    Ok(done??)
}

fn compressed(mut encoder: Encoder, plain: &[Bytes]) -> std::io::Result<(Encoder, Vec<u8>)> {
    use zstd::stream::raw::{InBuffer, Operation, OutBuffer};
    let mut out = Vec::new();
    let mut room = vec![0; zstd::zstd_safe::CCtx::out_size()];
    for part in plain {
        let mut input = InBuffer::around(part);
        while input.pos() < part.len() {
            let mut output = OutBuffer::around(&mut room[..]);
            encoder.run(&mut input, &mut output)?;
            let wrote = output.pos();
            out.extend_from_slice(&room[..wrote]);
        }
    }
    Ok((encoder, out))
}

fn finished(mut encoder: Encoder) -> std::io::Result<Vec<u8>> {
    use zstd::stream::raw::{Operation, OutBuffer};
    let mut out = Vec::new();
    loop {
        let mut room = vec![0; zstd::zstd_safe::CCtx::out_size()];
        let mut output = OutBuffer::around(&mut room[..]);
        let left = encoder.finish(&mut output, true)?;
        let wrote = output.pos();
        out.extend_from_slice(&room[..wrote]);
        if left == 0 {
            return Ok(out);
        }
    }
}

enum Read {
    Framed(Vec<u8>),
    Unnamed,
    Fetching,
}

impl Read {
    fn framed(self) -> Option<Vec<u8>> {
        match self {
            Read::Framed(framed) => Some(framed),
            Read::Unnamed | Read::Fetching => None,
        }
    }
}

async fn served<E: Context>(ctx: &Ctx<E>, id: BlobId) -> crate::Result<Read> {
    let view = ctx.view().clone();
    let now = ctx.now();
    let readable = offload(move || {
        tokio::runtime::Handle::current().block_on(view.source().readable(&id, now))
    })
    .await??;
    if !readable {
        return Ok(Read::Unnamed);
    }
    let store = ctx.blobs().store().clone();
    let held = store.has(&id) || ctx.blobs().seek(id).await;
    if !held {
        return Ok(Read::Fetching);
    }
    let framed = offload(move || store.framed_once(&id)).await??;
    Ok(framed.map_or(Read::Unnamed, Read::Framed))
}

fn encoded(read: crate::Result<Option<Vec<u8>>>) -> Vec<crate::Result<Bytes>> {
    let framed = match read {
        Ok(Some(framed)) => framed,
        Ok(None) => return vec![Ok(Bytes::from_static(&[0]))],
        Err(error) => {
            tracing::warn!(
                target: "ducktape::http",
                event = "blobs_cut",
                %error,
                reason = "blob_unread",
                "a blob answer stops mid-stream: a blob it lists could not be read"
            );
            return vec![Err(error)];
        }
    };
    let Ok(len) = u32::try_from(framed.len()) else {
        let over = std::io::Error::other("a framed blob is longer than a u32 length says");
        return vec![Err(over.into())];
    };
    let mut head = vec![1];
    head.extend_from_slice(&len.to_le_bytes());
    vec![Ok(Bytes::from(head)), Ok(Bytes::from(framed))]
}

async fn blob_put<E: Context>(ctx: Ctx<E>, request: Request) -> Reply<Submitted> {
    let stream = request
        .into_body()
        .into_data_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    let mut body = tokio_util::io::StreamReader::new(stream);
    let frame = match framed_put(&mut body).await {
        Ok(frame) => frame,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let submission = match node::verify(&frame, &ctx.network_id()) {
        Ok(submission) => submission,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let bound = ctx.view().source().roles().bound(role::BLOBS).cloned();
    let role = match bound {
        Ok(role) => role,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let entries = match put_entries(&role, &submission) {
        Ok(entries) => entries,
        Err(refusal) => return Reply::Refused(refusal),
    };
    if let Err(refusal) = ctx.blobs().stage(&entries, &mut body).await {
        return Reply::Refused(refusal);
    }
    taken(ctx.submit(frame, Preconfirm::Off).await)
}

async fn framed_put(body: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<Vec<u8>, Refusal> {
    use tokio::io::AsyncReadExt as _;
    let unread = |error: std::io::Error| {
        Refusal::new(
            reason::INVALID_INPUT,
            format!("the upload does not open with a framed put: {error}"),
        )
    };
    let len = body.read_u32_le().await.map_err(unread)? as usize;
    let fits = len <= SUBMIT_BYTES;
    if !fits {
        return Err(Refusal::new(
            reason::CAPACITY,
            format!("the put frame is {len} bytes and a frame is at most {SUBMIT_BYTES}"),
        ));
    }
    let mut frame = vec![0u8; len];
    body.read_exact(&mut frame).await.map_err(unread)?;
    Ok(frame)
}

fn put_entries(
    role: &str,
    submission: &host::Submission,
) -> Result<Vec<blob::contract::Entry>, Refusal> {
    let to_the_blobs_program = submission.target == role;
    if !to_the_blobs_program {
        return Err(Refusal::new(
            reason::INVALID_INPUT,
            format!(
                "an upload carries a put to {role}, not to {}",
                submission.target
            ),
        ));
    }
    match abi::decode(&submission.payload)? {
        blob::contract::Op::Put { blobs } => Ok(blobs),
        blob::contract::Op::Receipt { .. } => Err(Refusal::new(
            reason::INVALID_INPUT,
            "an upload carries a put, not a receipt",
        )),
    }
}

async fn blobs_held<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<Vec<bool>> {
    let ids: Vec<BlobId> = match abi::decode(&body) {
        Ok(ids) => ids,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let view = ctx.view().clone();
    let now = ctx.now();
    answered(
        offload(move || {
            let source = view.source();
            let handle = tokio::runtime::Handle::current();
            ids.iter()
                .map(|id| handle.block_on(source.readable(id, now)))
                .collect::<Result<Vec<bool>, _>>()
        })
        .await,
    )
}

async fn blob_missing<E: Context>(ctx: Ctx<E>) -> Reply<Vec<BlobId>> {
    let view = ctx.view().clone();
    answered(offload(move || view.source().missing_blobs()).await)
}

async fn programs<E: Context>(ctx: Ctx<E>) -> Response {
    let view = ctx.view().clone();
    match offload(move || view.programs()).await {
        Ok(Ok(encoded)) => Encoded(encoded).into_response(),
        Ok(Err(error)) => failed::<()>(error).into_response(),
        Err(error) => failed::<()>(error).into_response(),
    }
}

/// `/v1/changes/<program>`: each block that writes to `program`, as a
/// `Change`; `?keys` asks for the keys alone, every value `None` (a client
/// that re-reads what moved, as the app does, never reads the values). A
/// block that writes nothing there is a ping, and so is the block that
/// closes each epoch. Every ping carries its block's height; a client that
/// stops draining for a whole epoch is cut (`changes::written`).
async fn changes<E: Context>(
    ctx: Ctx<E>,
    Path(program): Path<ProgramId>,
    RawQuery(query): RawQuery,
    upgrade: WebSocketUpgrade,
) -> Response {
    let keys = keys_asked(query.as_deref());
    let subscription = ctx.subscribe(program, keys);
    upgrade.on_upgrade(move |socket| stream(socket, subscription))
}

async fn watching<E: Context>(
    ctx: Ctx<E>,
    Path(program): Path<ProgramId>,
    RawQuery(query): RawQuery,
    upgrade: WebSocketUpgrade,
) -> Response {
    let keys = keys_asked(query.as_deref());
    let watching = ctx.watch(program, keys);
    upgrade.on_upgrade(move |socket| crate::standing::serve(socket, watching))
}

fn keys_asked(query: Option<&str>) -> bool {
    query.is_some_and(|query| query.split('&').any(|asked| asked == "keys"))
}

async fn heads<E: Context>(ctx: Ctx<E>, upgrade: WebSocketUpgrade) -> Response {
    let subscription = ctx.subscribe_heads();
    upgrade.on_upgrade(move |socket| stream(socket, subscription))
}

enum Next {
    Published(Published),
    Frame,
    Stopping,
    Stopped,
    Ended,
}

async fn stream(socket: WebSocket, subscription: Subscription) {
    let Subscription {
        mut changes,
        writing,
        mut cut,
    } = subscription;
    // the node is stopping: what is queued still goes, then the goodbye
    let mut stopping = false;
    let (mut client, mut frames) = socket.split();
    loop {
        let next = tokio::select! {
            dropped = &mut cut, if !stopping => match dropped {
                Ok(()) => Next::Stopping,
                Err(_) => Next::Ended,
            },
            published = changes.next() => published.map_or(Next::Stopped, Next::Published),
            frame = frames.next() => match frame {
                Some(Ok(_)) => Next::Frame,
                Some(Err(_)) | None => Next::Ended,
            },
        };
        let (height, message) = match next {
            Next::Published(Published::Change(change)) => {
                (change.height, Message::Binary(abi::encode(&change).into()))
            }
            Next::Published(Published::Head(head)) => {
                (head.height, Message::Binary(abi::encode(&head).into()))
            }
            Next::Published(Published::Ping(height)) => (
                height,
                Message::Ping(Bytes::copy_from_slice(&height.to_le_bytes())),
            ),
            Next::Stopping => {
                stopping = true;
                continue;
            }
            Next::Stopped => return goodbye(client).await,
            Next::Frame => continue,
            Next::Ended => return,
        };
        // a client that stopped reading holds the send: its cut ends it
        writing.begin(height);
        let send = client.send(message);
        pin_mut!(send);
        loop {
            tokio::select! {
                sent = &mut send => match sent {
                    Ok(()) => break,
                    Err(_) => return,
                },
                dropped = &mut cut, if !stopping => match dropped {
                    Ok(()) => stopping = true,
                    Err(_) => return,
                },
            }
        }
        writing.end();
    }
}

pub(crate) async fn goodbye(mut client: SplitSink<WebSocket, Message>) {
    let stopped = CloseFrame {
        code: close_code::AWAY,
        reason: "the node stopped".into(),
    };
    let _ = client.send(Message::Close(Some(stopped))).await;
}

async fn blocks<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<Page> {
    let page: Blocks = match abi::decode(&body) {
        Ok(page) => page,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match ctx.blocks(page).await {
        Ok(page) => Reply::Answered(page),
        Err(error) => failed(error),
    }
}

async fn block<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<Option<Finalized>> {
    let ask: BlockAsk = match abi::decode(&body) {
        Ok(ask) => ask,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match ctx.block(ask).await {
        Ok(block) => Reply::Answered(block),
        Err(error) => failed(error),
    }
}

async fn tx<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<Option<Located>> {
    let ask: TxAsk = match abi::decode(&body) {
        Ok(ask) => ask,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match ctx.tx(ask).await {
        Ok(tx) => Reply::Answered(tx),
        Err(error) => failed(error),
    }
}

async fn activity<E: Context>(
    ctx: Ctx<E>,
    Whole(body): Whole,
) -> Reply<crate::wire::activity::Reply> {
    let query: crate::wire::activity::Query = match abi::decode(&body) {
        Ok(query) => query,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match ctx.activity(query).await {
        Ok(Ok(reply)) => Reply::Answered(reply),
        Ok(Err(refusal)) => Reply::Refused(refusal),
        Err(error) => failed(error),
    }
}

async fn logs<E: Context>(ctx: Ctx<E>) -> Reply<Vec<String>> {
    Reply::Answered(ctx.logs())
}

async fn admin<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<()> {
    let submission = match node::verify(&body, &ctx.network_id()) {
        Ok(submission) => submission,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let signed_by_the_node = submission.signer == ctx.identity();
    let addressed_to_admin = submission.target == crate::wire::ADMIN;
    if !(signed_by_the_node && addressed_to_admin) {
        return Reply::Refused(Refusal::new(
            reason::INVALID_INPUT,
            "an admin verb is signed by the node's own key",
        ));
    }
    let fresh = match ctx.view().source().fresh(&submission) {
        Ok(fresh) => fresh,
        Err(error) => return failed(error),
    };
    if let Err(refusal) = fresh {
        return Reply::Refused(refusal);
    }
    let verb: Admin = match abi::decode(&submission.payload) {
        Ok(verb) => verb,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match verb {
        Admin::Shutdown => {
            ctx.shut_down();
            Reply::Answered(())
        }
        Admin::LogFilter(directives) => match ctx.retune(&directives) {
            Ok(()) => Reply::Answered(()),
            Err(error) => Reply::Refused(Refusal::new(reason::INVALID_INPUT, error)),
        },
        Admin::EgressReload => match ctx.reload_egress() {
            Ok(()) => Reply::Answered(()),
            Err(refused) => Reply::Refused(Refusal::new(refused.reason(), refused.to_string())),
        },
    }
}

async fn egress_set<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<EgressPlan> {
    let submission = match node::verify(&body, &ctx.network_id()) {
        Ok(submission) => submission,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let addressed_to_egress = submission.target == crate::wire::EGRESS;
    if !addressed_to_egress {
        return Reply::Refused(Refusal::new(
            reason::INVALID_INPUT,
            "an egress set is addressed to $egress",
        ));
    }
    let signer = match ctx.egress_signer(&submission.signer) {
        Ok(signer) => signer,
        Err(error) => {
            return Reply::Refused(Refusal::new(
                reason::CORRUPT,
                format!("the workspace's egress-grants does not read: {error}"),
            ));
        }
    };
    if let EgressSigner::Stranger = signer {
        return Reply::Refused(Refusal::new(
            reason::UNAUTHORIZED,
            "an egress set is signed by the node's own key or a key the workspace's egress-grants lists",
        ));
    }
    let fresh = match ctx.view().source().fresh(&submission) {
        Ok(fresh) => fresh,
        Err(error) => return failed(error),
    };
    if let Err(refusal) = fresh {
        return Reply::Refused(refusal);
    }
    let asked: EgressSet = match abi::decode(&submission.payload) {
        Ok(asked) => asked,
        Err(refusal) => return Reply::Refused(refusal),
    };
    if let Some(refusal) = withheld_from(&signer, &asked.text) {
        return Reply::Refused(refusal);
    }
    let planned = match ctx.plan_egress(asked.text) {
        Ok(planned) => planned,
        Err(refused) => return Reply::Refused(Refusal::new(refused.reason(), refused.to_string())),
    };
    let reach = planned.reach(ctx.egress_wait()).await;
    let tally = match asked.dry {
        true => planned.tally,
        false => match ctx.put_egress(planned) {
            Ok(tally) => tally,
            Err(refused) => {
                return Reply::Refused(Refusal::new(refused.reason(), refused.to_string()));
            }
        },
    };
    Reply::Answered(EgressPlan {
        served: tally.served as u64,
        added: tally.added as u64,
        removed: tally.removed as u64,
        changed: tally.changed as u64,
        reach,
    })
}

fn withheld_from(signer: &EgressSigner, text: &str) -> Option<Refusal> {
    let found = match crate::egress::withheld(text) {
        Ok(found) => found,
        Err(refused) => return Some(Refusal::new(refused.reason(), refused.to_string())),
    };
    found
        .into_iter()
        .find(|withheld| refused_for(signer, withheld))
        .map(|withheld| Refusal::new(withheld.reason(), withheld.to_string()))
}

fn refused_for(signer: &EgressSigner, withheld: &crate::egress::Withheld) -> bool {
    match (withheld, signer) {
        (crate::egress::Withheld::Secrets { .. }, _) => true,
        (crate::egress::Withheld::Private, EgressSigner::Granted) => true,
        (crate::egress::Withheld::Private, EgressSigner::Node | EgressSigner::Stranger) => false,
    }
}

async fn sync<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Reply<statesync::Response> {
    let request: SyncRequest = match abi::decode(&body) {
        Ok(request) => request,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match ctx.sync(request).await {
        Ok(response) => Reply::Answered(response),
        Err(error) => failed(error),
    }
}

struct Open {
    tips: tokio::sync::watch::Receiver<u64>,
    held: Held,
}

struct Held {
    _hold: statesync::Holding,
    height: u64,
    opened: Instant,
    ended: &'static str,
    tally: Arc<AtomicU64>,
}

impl Held {
    fn new(held: crate::HeldHead, opened: Instant, tally: Arc<AtomicU64>) -> Held {
        Held {
            _hold: held.hold,
            height: held.height,
            opened,
            ended: "connection_ended",
            tally,
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        tracing::debug!(
            target: "ducktape::sync",
            event = "sync_hold_ended",
            height = self.height,
            open_ms = self.opened.elapsed().as_millis() as u64,
            reason = self.ended,
            "a join's hold on this node's state ended"
        );
        self.tally.fetch_add(1, Ordering::Relaxed);
    }
}

async fn sync_hold<E: Context>(ctx: Ctx<E>) -> Response {
    let opened = Instant::now();
    let (head, held) = match ctx.held_head().await {
        Ok(held) => held,
        Err(error) => return failed::<()>(error).into_response(),
    };
    let head = Bytes::from(length_framed(&abi::encode(&head)));
    let Some(held) = held else {
        return (StatusCode::OK, head).into_response();
    };
    let number = held.number.to_string();
    let open = Open {
        tips: ctx.tips(),
        held: Held::new(held, opened, ctx.holds_ended()),
    };
    let heights = futures::stream::unfold(open, applied);
    let answer = futures::stream::once(async move { Ok::<_, std::io::Error>(head) }).chain(heights);
    let named = [(crate::wire::HOLD_HEADER, number)];
    (StatusCode::OK, named, Body::from_stream(answer)).into_response()
}

async fn applied(mut open: Open) -> Option<(std::io::Result<Bytes>, Open)> {
    if open.tips.changed().await.is_err() {
        open.held.ended = "node_stopped";
        return None;
    }
    let height = *open.tips.borrow_and_update();
    let line = Bytes::from(length_framed(&height.to_le_bytes()));
    Some((Ok(line), open))
}

fn length_framed(bytes: &[u8]) -> Vec<u8> {
    let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(bytes);
    framed
}

async fn snapshot<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Response {
    let ask: crate::wire::snapshot::Ask = match abi::decode(&body) {
        Ok(ask) => ask,
        Err(refusal) => return Reply::<()>::Refused(refusal).into_response(),
    };
    let Some(kept) = ctx.snapshot() else {
        let none = Refusal::new(reason::NOT_FOUND, "this node keeps no snapshot");
        return Reply::<()>::Refused(none).into_response();
    };
    let answer = crate::snapshot::served(kept, ask);
    (StatusCode::OK, Body::from_stream(answer)).into_response()
}

async fn metrics<E: Context>(ctx: Ctx<E>) -> Response {
    match ctx.metrics().await {
        Ok(text) => text.into_response(),
        Err(error) => failed::<()>(error).into_response(),
    }
}

async fn p2p<E: Context>(ctx: Ctx<E>, upgrade: WebSocketUpgrade) -> Response {
    let mesh = ctx.mesh();
    upgrade.on_upgrade(move |socket| crate::transport::inbound_websocket(socket, mesh))
}

/// The UDP port a client dials for live tracks, `None` when the node runs no QUIC door.
async fn live_port<E: Context>(ctx: Ctx<E>) -> Reply<Option<u16>> {
    Reply::Answered(ctx.live_port())
}

async fn halo_self<E: Context>(ctx: Ctx<E>) -> Reply<crate::wire::HaloSelf> {
    Reply::Answered(ctx.halo_self())
}

async fn live_socket<E: Context>(ctx: Ctx<E>, upgrade: WebSocketUpgrade) -> Response {
    let live = ctx.live();
    crate::live::ws_upgrade(upgrade).on_upgrade(move |socket| live.serve_ws(socket))
}

async fn halo_relay<E: Context>(ctx: Ctx<E>, upgrade: WebSocketUpgrade) -> Response {
    let halo = ctx.halo();
    upgrade.on_upgrade(move |socket| relayed(socket, halo))
}

async fn forward<E: Context>(ctx: Ctx<E>, upgrade: WebSocketUpgrade) -> Response {
    let daemon = ctx.forwarding();
    upgrade.on_upgrade(move |socket| crate::forward::called(socket, daemon))
}

async fn relayed(socket: WebSocket, halo: Arc<halo::Halo>) {
    if let Err(error) = halo.relayed(piped(socket)).await {
        tracing::debug!(
            target: "ducktape::halo",
            %error,
            reason = "unrelayed",
            "a halo link carried on this node's http did not come up"
        );
    }
}

fn piped(socket: WebSocket) -> halo::Pipe {
    let (sink, stream) = socket.split();
    let send = sink
        .sink_map_err(std::io::Error::other)
        .with(|packet: Bytes| {
            futures::future::ready(Ok::<Message, std::io::Error>(Message::Binary(packet)))
        });
    let recv = stream.filter_map(|message| {
        futures::future::ready(match message {
            Ok(Message::Binary(packet)) => Some(Ok(packet)),
            Ok(Message::Ping(_) | Message::Pong(_)) => None,
            Ok(Message::Text(_) | Message::Close(_)) => {
                Some(Err(std::io::ErrorKind::ConnectionAborted.into()))
            }
            Err(error) => Some(Err(std::io::Error::other(error))),
        })
    });
    halo::Pipe {
        send: Box::pin(send),
        recv: Box::pin(recv),
    }
}

const CLIENT_IP: &str = "cf-connecting-ip";

async fn seen(ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> String {
    seen_from(&headers, peer).to_string()
}

fn seen_from(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    headers
        .get(CLIENT_IP)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(peer.ip().to_canonical())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::body::Body;

    use super::*;

    const CAP: usize = 16;

    async fn read(bytes: usize) -> Result<Capped<CAP>, Reply<()>> {
        let request = Request::new(Body::from(vec![0; bytes]));
        Capped::<CAP>::from_request(request, &()).await
    }

    #[tokio::test]
    async fn a_body_within_its_cap_is_read_whole() {
        let Ok(Capped(body)) = read(CAP).await else {
            panic!("a body at the cap is read");
        };
        assert_eq!(body.len(), CAP);
    }

    #[tokio::test]
    async fn a_body_over_its_cap_is_refused_for_capacity() {
        let Err(Reply::Refused(refusal)) = read(CAP + 1).await else {
            panic!("a body over the cap is refused");
        };
        assert_eq!(refusal.reason, reason::CAPACITY);
    }

    #[tokio::test]
    async fn seen_answers_the_cloudflare_client_ip_when_it_reads_and_else_the_peer() {
        let router = Router::new().route(halo::SEEN_ROUTE, get(seen));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        });
        let url = format!("http://{address}{}", halo::SEEN_ROUTE);
        let client = reqwest::Client::new();
        let mut answers = Vec::new();
        for header in [
            None,
            Some("203.0.113.9"),
            Some(" 2001:db8::7 "),
            Some("not an ip"),
        ] {
            let mut asked = client.get(&url);
            if let Some(value) = header {
                asked = asked.header(CLIENT_IP, value);
            }
            answers.push(asked.send().await.unwrap().text().await.unwrap());
        }
        assert_eq!(
            answers,
            ["127.0.0.1", "203.0.113.9", "2001:db8::7", "127.0.0.1"]
        );
    }

    #[test]
    fn a_streamed_blob_answer_is_the_bytes_of_the_whole_one() {
        let page = vec![Some(b"page 1\x00a".to_vec()), None, Some(Vec::new())];
        let mut streamed = (page.len() as u32).to_le_bytes().to_vec();
        for read in &page {
            for part in encoded(Ok(read.clone())) {
                streamed.extend_from_slice(&part.unwrap());
            }
        }
        assert_eq!(streamed, abi::encode(&page));
    }

    const PAST_AXUM_DEFAULT_LIMIT: usize = (2 << 20) + 1;

    #[tokio::test]
    async fn a_body_past_the_frameworks_default_limit_is_read_whole() {
        let router = Router::new().route(
            "/whole",
            post(|Whole(body): Whole| async move { body.len().to_string() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        let response = reqwest::Client::new()
            .post(format!("http://{address}/whole"))
            .body(vec![0u8; PAST_AXUM_DEFAULT_LIMIT])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let read = response.text().await.unwrap();
        assert_eq!(read, PAST_AXUM_DEFAULT_LIMIT.to_string());
    }

    struct Caught(
        &'static str,
        tokio::sync::mpsc::UnboundedSender<BTreeMap<String, String>>,
    );

    struct Fields(BTreeMap<String, String>);

    impl tracing::field::Visit for Fields {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Caught {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields(BTreeMap::new());
            event.record(&mut fields);
            fields
                .0
                .insert("level".to_owned(), event.metadata().level().to_string());
            let named = fields.0.get("event").is_some_and(|event| event == self.0);
            if named {
                let _ = self.1.send(fields.0);
            }
        }
    }

    const BLOCK: Duration = Duration::from_millis(250);

    #[test]
    fn slow_requests_in_one_block_make_one_line_with_their_count_and_a_quiet_block_none() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let (caught, mut lines) = tokio::sync::mpsc::unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(Caught("request_slow", caught));
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (answered, answers) = tokio::sync::mpsc::unbounded_channel();
                let (tips, tipped) = tokio::sync::watch::channel(0);
                let lining = tokio::spawn(slow_lines(BLOCK, answers, tipped, Arc::default()));
                let requests = [
                    (BLOCK * 2, "/v1/query"),
                    (BLOCK * 5, "/v1/tx"),
                    (BLOCK * 3, "/v1/query"),
                    (BLOCK, "/v1/status"),
                ];
                for (spent, route) in requests {
                    let route = route.to_owned();
                    answered.send(Answered { route, spent }).unwrap();
                }
                tips.send_replace(1);
                let line = lines.recv().await.unwrap();
                assert_eq!(line["height"], "1");
                assert_eq!(line["count"], "3");
                assert_eq!(line["max_ms"], (BLOCK * 5).as_millis().to_string());
                assert_eq!(line["slowest"], "/v1/tx");
                tips.send_replace(2);
                drop(tips);
                lining.await.unwrap();
                assert!(
                    lines.try_recv().is_err(),
                    "a block with no slow request writes nothing"
                );
            });
        });
    }

    fn open(tips: tokio::sync::watch::Receiver<u64>, height: u64, tally: &Arc<AtomicU64>) -> Open {
        let held = crate::HeldHead {
            height,
            number: 0,
            hold: Box::new(()),
        };
        Open {
            tips,
            held: Held::new(held, Instant::now(), tally.clone()),
        }
    }

    #[test]
    fn a_hold_says_once_when_it_ends_at_which_height_and_why() {
        use futures::StreamExt as _;
        use tracing_subscriber::layer::SubscriberExt as _;
        let (caught, mut lines) = tokio::sync::mpsc::unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(Caught("sync_hold_ended", caught));
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let tally: Arc<AtomicU64> = Arc::default();
                let (tips, _) = tokio::sync::watch::channel(7);
                let mut heights = Box::pin(futures::stream::unfold(
                    open(tips.subscribe(), 7, &tally),
                    applied,
                ));
                tips.send_replace(8);
                assert!(heights.next().await.is_some());
                assert!(lines.try_recv().is_err(), "an open hold says nothing");
                drop(heights);
                let line = lines.recv().await.unwrap();
                assert_eq!(line["level"], "DEBUG");
                assert_eq!(line["height"], "7");
                assert_eq!(line["reason"], "connection_ended");
                assert!(line.contains_key("open_ms"));
                let heights = futures::stream::unfold(open(tips.subscribe(), 8, &tally), applied);
                drop(tips);
                assert_eq!(heights.count().await, 0);
                let line = lines.recv().await.unwrap();
                assert_eq!(line["height"], "8");
                assert_eq!(line["reason"], "node_stopped");
                assert!(lines.try_recv().is_err(), "each hold says it once");
                assert_eq!(tally.load(Ordering::Relaxed), 2);
            });
        });
    }

    #[test]
    fn holds_ended_in_one_block_make_one_info_line_with_their_count_and_a_quiet_block_none() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let (caught, mut lines) = tokio::sync::mpsc::unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(Caught("sync_holds_ended", caught));
        tracing::subscriber::with_default(subscriber, || {
            let (tips, _) = tokio::sync::watch::channel(0);
            let tally: Arc<AtomicU64> = Arc::default();
            for height in 0..3 {
                drop(open(tips.subscribe(), height, &tally));
            }
            holds_closed(&tally, 5);
            let line = lines.try_recv().unwrap();
            assert_eq!(line["level"], "INFO");
            assert_eq!(line["height"], "5");
            assert_eq!(line["count"], "3");
            holds_closed(&tally, 6);
            assert!(
                lines.try_recv().is_err(),
                "a block with no ended hold writes nothing"
            );
        });
    }
}
