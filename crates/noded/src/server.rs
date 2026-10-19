use std::sync::Arc;
use std::sync::atomic::Ordering;

use abi::Receipt;
use abi::{BlobId, Origin, ProgramId, Refusal, reason, role};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{FromRequest, Path, RawQuery, Request};
use axum::http::StatusCode;
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
        fanout: Fanout,
        rpc_cpus: Option<Vec<usize>>,
    ) -> crate::Result<Rpc> {
        let pinned = rpc_cpus.is_some();
        let cpus = match rpc_cpus {
            Some(cpus) => cpus,
            None => cores::usable()?,
        };
        let threads = cpus.len();
        tracing::info!(target: "ducktape::http", event = "rpc_cores", rpc = threads, pinned, "the rpc runtime runs beside the block threads");
        let cpus = Arc::new(cpus);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("rpc")
            .on_thread_start(move || yielding(&cpus))
            .enable_all()
            .build()?;
        let router = router(daemon.clone());
        runtime.spawn(changes::fan(fanout));
        for _ in 0..threads {
            let door = listener.try_clone()?;
            runtime.spawn(accepting(door, router.clone(), daemon.shutdown.subscribe()));
        }
        Ok(Rpc(Some(runtime)))
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
    let served = axum::serve(listener, router)
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

pub fn router<E: Context>(daemon: Arc<Daemon<E>>) -> Router {
    Router::new()
        .route(route::STATUS, get(status::<E>))
        .route(route::NETWORK, get(network::<E>))
        .route(route::GET, post(get_value::<E>))
        .route(route::SCAN, post(scan::<E>))
        .route(route::BLOB_GET, post(blob_get::<E>))
        .route(route::BLOBS, post(blobs::<E>))
        .route(route::BLOB_MISSING, get(blob_missing::<E>))
        .route(route::PROGRAMS, get(programs::<E>))
        .route(route::BLOCKS, post(blocks::<E>))
        .route(route::BLOCK, post(block::<E>))
        .route(route::TX, post(tx::<E>))
        .route(route::ACTIVITY, post(activity::<E>))
        .route(route::LOGS, get(logs::<E>))
        .route(route::ADMIN, post(admin::<E>))
        .route(route::EGRESS_SET, post(egress_set::<E>))
        .route(route::EGRESS_LEND, get(egress_lend::<E>))
        .route(route::SYNC, post(sync::<E>))
        .route(route::METRICS, get(metrics::<E>))
        .route(live_wire::PORT_ROUTE, get(live_port::<E>))
        .route(route::QUERY, post(query::<E>))
        .route(route::SUBMIT, post(submit::<E>))
        .route(route::BLOB_PUT, post(blob_put::<E>))
        .route(
            &format!("{}/{{program}}", route::CHANGES),
            get(changes::<E>),
        )
        .route(route::P2P, get(p2p::<E>))
        .route(live_wire::ROUTE, get(live_socket::<E>))
        .with_state(daemon)
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
    let asked = offload(move || {
        let submission = match node::verify(&query.frame, &network) {
            Ok(submission) => submission,
            Err(refusal) => return Asked::Unverified(refusal),
        };
        let source = reading.source();
        let answer = source.query(
            query.layer,
            time,
            Origin::External(submission.signer),
            &submission.target,
            submission.payload,
        );
        Asked::Answered(tokio::runtime::Handle::current().block_on(answer))
    })
    .await;
    match asked {
        Ok(Asked::Unverified(refusal)) => Reply::Refused(refusal),
        Ok(Asked::Answered(Ok(Ok(bytes)))) => Reply::Answered(bytes),
        Ok(Asked::Answered(Ok(Err(refusal)))) => Reply::Refused(refusal),
        Ok(Asked::Answered(Err(error))) => failed(error),
        Err(error) => failed(error),
    }
}

enum Asked {
    Unverified(Refusal),
    Answered(host::Result<Result<Vec<u8>, Refusal>>),
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
    answered(offload(move || reading.source().view(get.layer).get(&get.program, &get.key)).await)
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
            reading
                .source()
                .view(range.layer)
                .scan(&range.program, &range.scan)
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
        Ok(framed) => Reply::Answered(framed),
        Err(error) => failed(error),
    }
}

async fn blobs<E: Context>(ctx: Ctx<E>, Whole(body): Whole) -> Response {
    let ids: Vec<BlobId> = match abi::decode(&body) {
        Ok(ids) => ids,
        Err(refusal) => return Reply::<()>::Refused(refusal).into_response(),
    };
    let count = Bytes::copy_from_slice(&(ids.len() as u32).to_le_bytes());
    let items = futures::stream::iter(ids)
        .then(move |id| {
            let ctx = ctx.clone();
            async move { served(&ctx, id).await }
        })
        .flat_map(|read| futures::stream::iter(encoded(read)));
    let answer = futures::stream::once(async move { Ok(count) }).chain(items);
    (StatusCode::OK, Body::from_stream(answer)).into_response()
}

async fn served<E: Context>(ctx: &Ctx<E>, id: BlobId) -> crate::Result<Option<Vec<u8>>> {
    let view = ctx.view().clone();
    let now = ctx.now();
    let readable = offload(move || {
        tokio::runtime::Handle::current().block_on(view.source().readable(&id, now))
    })
    .await??;
    if !readable {
        return Ok(None);
    }
    let store = ctx.blobs().store().clone();
    if !store.has(&id) {
        ctx.fetch(id).await;
    }
    Ok(offload(move || store.framed_once(&id)).await??)
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
/// answers none of them for a whole epoch is cut (`Daemon::publish`).
async fn changes<E: Context>(
    ctx: Ctx<E>,
    Path(program): Path<ProgramId>,
    RawQuery(query): RawQuery,
    upgrade: WebSocketUpgrade,
) -> Response {
    let keys = query
        .as_deref()
        .is_some_and(|query| query.split('&').any(|asked| asked == "keys"));
    let subscription = ctx.subscribe(program, keys);
    upgrade.on_upgrade(move |socket| stream(socket, subscription))
}

enum Next {
    Published(Published),
    Pong(Bytes),
    Frame,
    Stopping,
    Stopped,
    Ended,
}

async fn stream(socket: WebSocket, subscription: Subscription) {
    let Subscription {
        mut changes,
        read,
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
                Some(Ok(Message::Pong(echo))) => Next::Pong(echo),
                Some(Ok(_)) => Next::Frame,
                Some(Err(_)) | None => Next::Ended,
            },
        };
        let message = match next {
            Next::Published(Published::Change(change)) => {
                Message::Binary(abi::encode(&change).into())
            }
            Next::Published(Published::Ping(height)) => {
                Message::Ping(Bytes::copy_from_slice(&height.to_le_bytes()))
            }
            Next::Pong(echo) => {
                if let Ok(height) = <[u8; 8]>::try_from(echo.as_ref()) {
                    read.fetch_max(u64::from_le_bytes(height), Ordering::Relaxed);
                }
                continue;
            }
            Next::Stopping => {
                stopping = true;
                continue;
            }
            Next::Stopped => return goodbye(client).await,
            Next::Frame => continue,
            Next::Ended => return,
        };
        // a client that stopped reading holds the send: its cut ends it
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
    }
}

async fn goodbye(mut client: SplitSink<WebSocket, Message>) {
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

async fn egress_lend<E: Context>(ctx: Ctx<E>, upgrade: WebSocketUpgrade) -> Response {
    let daemon = ctx.daemon();
    upgrade
        .max_message_size(usize::MAX)
        .max_frame_size(usize::MAX)
        .on_upgrade(move |socket| crate::egress::lend_to(daemon, socket))
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

async fn live_socket<E: Context>(ctx: Ctx<E>, upgrade: WebSocketUpgrade) -> Response {
    let live = ctx.live();
    crate::live::ws_upgrade(upgrade).on_upgrade(move |socket| live.serve_ws(socket))
}

#[cfg(test)]
mod tests {
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
}
