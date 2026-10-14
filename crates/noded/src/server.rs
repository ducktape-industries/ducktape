use std::sync::Arc;
use std::time::Duration;

use abi::{BlobId, Origin, ProgramId, Refusal, reason};
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use blobs::Blobs;
use borsh::BorshSerialize;
use futures::channel::mpsc;
use futures::{SinkExt as _, StreamExt as _};
use halo::Class;
use host::attach::ATTACHMENT_BYTES;
use host::{Host, Receipt};
use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use node::{BLOCK_BYTES, Source, Taken};
use statesync::Request as SyncRequest;
use tokio::sync::Semaphore;

use crate::wire::{
    Admin, BlobPut, BlockRef, Blocks, Change, Finalized, Get, Located, Network, PAGE_BYTES, Page,
    QUERY_BYTES, Query, Range, TxAsk, route,
};
use crate::{Context, Daemon, Error, fetch};

const READS_IN_FLIGHT: usize = 64;
const READ_DEADLINE: Duration = Duration::from_secs(30);
const UPLOADS_IN_FLIGHT: usize = 8;
const UPLOAD_DEADLINE: Duration = Duration::from_secs(300);

const SUBMIT_BYTES: usize = BLOCK_BYTES;
const BLOB_PUT_BYTES: usize = ATTACHMENT_BYTES as usize + (1 << 10);
const ASK_BYTES: usize = 1 << 20;

pub fn router<E: Context>(daemon: Arc<Daemon<E>>) -> Router {
    let read_permits = Arc::new(Semaphore::new(READS_IN_FLIGHT));
    let reads = Router::new()
        .route(route::STATUS, get(status::<E>))
        .route(route::NETWORK, get(network::<E>))
        .route(route::GET, post(get_value::<E>))
        .route(route::SCAN, post(scan::<E>))
        .route(route::QUERY, post(query::<E>))
        .route(route::BLOB_GET, post(blob_get::<E>))
        .route(route::BLOB_MISSING, get(blob_missing::<E>))
        .route(route::BLOBS, post(blobs::<E>))
        .route(route::PROGRAMS, get(programs::<E>))
        .route(route::BLOCKS, post(blocks::<E>))
        .route(route::BLOCK, post(block::<E>))
        .route(route::TX, post(tx::<E>))
        .route(route::LOGS, get(logs::<E>))
        .route(route::ADMIN, post(admin::<E>))
        .route(route::SYNC, post(sync::<E>))
        .route(route::METRICS, get(metrics::<E>))
        .route(live_wire::PORT_ROUTE, get(live_port::<E>))
        .layer(middleware::from_fn_with_state(
            Lane::new(read_permits, READ_DEADLINE),
            bounded,
        ));
    let uploads = Router::new()
        .route(route::SUBMIT, post(submit::<E>))
        .route(route::BLOB_PUT, post(blob_put::<E>))
        .layer(middleware::from_fn_with_state(
            Lane::new(Arc::new(Semaphore::new(UPLOADS_IN_FLIGHT)), UPLOAD_DEADLINE),
            bounded,
        ));
    let streams = Router::new()
        .route(
            &format!("{}/{{program}}", route::CHANGES),
            get(changes::<E>),
        )
        .route(route::P2P, get(p2p::<E>))
        .route(halo::ROUTE, get(halo_socket::<E>))
        .route(live_wire::ROUTE, get(live_socket::<E>));
    reads.merge(uploads).merge(streams).with_state(daemon)
}

#[derive(Clone)]
struct Lane {
    permits: Arc<Semaphore>,
    deadline: Duration,
}

impl Lane {
    fn new(permits: Arc<Semaphore>, deadline: Duration) -> Lane {
        Lane { permits, deadline }
    }
}

async fn bounded(State(lane): State<Lane>, request: Request, next: middleware::Next) -> Response {
    let deadline = tokio::time::Instant::now() + lane.deadline;
    let seconds = lane.deadline.as_secs();
    let permit = tokio::time::timeout_at(deadline, lane.permits.acquire_owned()).await;
    let Ok(Ok(_permit)) = permit else {
        let sentence =
            format!("the node is busy and did not start this request within {seconds} s");
        return (StatusCode::SERVICE_UNAVAILABLE, sentence).into_response();
    };
    match tokio::time::timeout_at(deadline, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            let sentence = format!("the node did not answer within {seconds} s");
            (StatusCode::SERVICE_UNAVAILABLE, sentence).into_response()
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
            Err(error) => Err(Reply::Refused(Refusal::new(
                reason::INVALID_INPUT,
                format!("the request body did not arrive whole: {error}"),
            ))),
        }
    }
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

async fn status<E: Context>(State(daemon): State<Arc<Daemon<E>>>) -> Reply<crate::wire::Status> {
    match daemon.status().await {
        Ok(status) => Reply::Answered(status),
        Err(error) => failed(error),
    }
}

async fn network<E: Context>(State(daemon): State<Arc<Daemon<E>>>) -> Reply<Network> {
    match daemon.network().await {
        Ok(network) => Reply::Answered(network),
        Err(error) => failed(error),
    }
}

async fn submit<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<SUBMIT_BYTES>,
) -> Reply<Receipt> {
    let mut node = daemon.node.lock().await;
    match node.submit(body.to_vec(), Source::Client).await {
        Ok(Ok(Taken::New(receipt))) => {
            drop(node);
            daemon.relay.send([body.as_ref()]);
            Reply::Answered(receipt)
        }
        Ok(Ok(Taken::Held(receipt))) => Reply::Answered(receipt),
        Ok(Err(refusal)) => Reply::Refused(refusal),
        Err(error) => failed(error),
    }
}

async fn query<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<QUERY_BYTES>,
) -> Reply<Vec<u8>> {
    let query: Query = match abi::decode(&body) {
        Ok(query) => query,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let submission = match node::verify(&query.frame, &daemon.descriptor.id()) {
        Ok(submission) => submission,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let node = daemon.node.lock().await;
    let answer = node
        .query(
            query.layer,
            Origin::External(submission.signer),
            &submission.target,
            submission.payload,
        )
        .await;
    let bytes = match answer {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(refusal)) => return Reply::Refused(refusal),
        Err(error) => return failed(error),
    };
    let over_the_cap = bytes.len() > QUERY_BYTES;
    if over_the_cap {
        let sentence = format!(
            "the answer is {} bytes and a query answers at most {QUERY_BYTES}: ask for a smaller page",
            bytes.len()
        );
        return Reply::Refused(Refusal::new(reason::CAPACITY, sentence));
    }
    Reply::Answered(bytes)
}

async fn get_value<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<ASK_BYTES>,
) -> Reply<Option<Vec<u8>>> {
    let get: Get = match abi::decode(&body) {
        Ok(get) => get,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let node = daemon.node.lock().await;
    match node.view(get.layer).get(&get.program, &get.key) {
        Ok(value) => Reply::Answered(value),
        Err(error) => failed(error),
    }
}

async fn scan<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<ASK_BYTES>,
) -> Reply<Vec<abi::Entry>> {
    let range: Range = match abi::decode(&body) {
        Ok(range) => range,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let node = daemon.node.lock().await;
    match node.view(range.layer).scan(&range.program, &range.scan) {
        Ok(entries) => Reply::Answered(entries),
        Err(error) => failed(error),
    }
}

async fn blob_get<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<ASK_BYTES>,
) -> Reply<Option<Vec<u8>>> {
    let id: BlobId = match abi::decode(&body) {
        Ok(id) => id,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match served(&daemon, &[id]).await {
        Ok(page) => Reply::Answered(page.into_iter().next().flatten()),
        Err(error) => failed(error),
    }
}

async fn blobs<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<ASK_BYTES>,
) -> Reply<Vec<Option<Vec<u8>>>> {
    let ids: Vec<BlobId> = match abi::decode(&body) {
        Ok(ids) => ids,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match served(&daemon, &ids).await {
        Ok(page) => Reply::Answered(page),
        Err(error) => failed(error),
    }
}

enum Answer {
    Unrostered,
    Held,
    Missing,
}

async fn served<E: Context>(
    daemon: &Daemon<E>,
    ids: &[BlobId],
) -> crate::Result<Vec<Option<Vec<u8>>>> {
    let (answers, store) = {
        let node = daemon.node.lock().await;
        (answers(node.host(), ids)?, node.host().blobs().clone())
    };
    let mut page = Vec::with_capacity(answers.len());
    for (id, answer) in ids.iter().zip(answers) {
        let framed = match answer {
            Answer::Unrostered => None,
            Answer::Held => Some(held(&store, id)?),
            Answer::Missing => Some(fetched(daemon, &store, id).await?),
        };
        page.push(framed);
    }
    Ok(page)
}

async fn fetched<E: Context>(
    daemon: &Daemon<E>,
    store: &Blobs,
    id: &BlobId,
) -> crate::Result<Vec<u8>> {
    fetch::repair(&daemon.node, &*daemon.halo, *id, Class::Client).await?;
    held(store, id)
}

fn held(store: &Blobs, id: &BlobId) -> crate::Result<Vec<u8>> {
    let Some(framed) = store.framed(id)? else {
        return Err(Error::Corrupt(format!(
            "rostered blob {id:?} left the blob store"
        )));
    };
    Ok(framed)
}

fn answers<E: Context>(host: &Host<E>, ids: &[BlobId]) -> crate::Result<Vec<Answer>> {
    let mut answers = Vec::new();
    let mut bytes = 0;
    for id in ids {
        if !host.rostered(id)? {
            answers.push(Answer::Unrostered);
            continue;
        }
        let Some(size) = host.blobs().size(id)? else {
            let first = answers.is_empty();
            if first {
                answers.push(Answer::Missing);
            }
            break;
        };
        let carries_one = bytes > 0;
        let past_page = bytes + size > PAGE_BYTES as u64;
        if carries_one && past_page {
            break;
        }
        bytes += size;
        answers.push(Answer::Held);
    }
    Ok(answers)
}

async fn blob_put<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<BLOB_PUT_BYTES>,
) -> Reply<()> {
    let put: BlobPut = match abi::decode(&body) {
        Ok(put) => put,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let mut node = daemon.node.lock().await;
    match node.install(put.id, &put.framed) {
        Ok(()) => Reply::Answered(()),
        Err(node::Error::Host(host::Error::Corrupt(sentence))) => {
            Reply::Refused(Refusal::new(reason::INVALID_INPUT, sentence))
        }
        Err(error) => failed(error),
    }
}

async fn blob_missing<E: Context>(State(daemon): State<Arc<Daemon<E>>>) -> Reply<Vec<BlobId>> {
    let node = daemon.node.lock().await;
    match node.host().missing_blobs() {
        Ok(missing) => Reply::Answered(missing),
        Err(error) => failed(error),
    }
}

async fn programs<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
) -> Reply<std::collections::BTreeMap<ProgramId, BlobId>> {
    let node = daemon.node.lock().await;
    match node.host().programs() {
        Ok(programs) => Reply::Answered(programs),
        Err(error) => failed(error),
    }
}

async fn changes<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Path(program): Path<ProgramId>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let subscription = daemon.subscribe(program);
    upgrade.on_upgrade(move |socket| stream(socket, subscription))
}

enum Next {
    Change(Change),
    Frame,
    Ended,
}

async fn stream(socket: WebSocket, mut changes: mpsc::UnboundedReceiver<Change>) {
    let (mut client, mut frames) = socket.split();
    loop {
        let next = tokio::select! {
            change = changes.next() => change.map_or(Next::Ended, Next::Change),
            frame = frames.next() => match frame {
                Some(Ok(_)) => Next::Frame,
                Some(Err(_)) | None => Next::Ended,
            },
        };
        match next {
            Next::Change(change) => {
                let sent = client
                    .send(Message::Binary(abi::encode(&change).into()))
                    .await;
                if sent.is_err() {
                    return;
                }
            }
            Next::Frame => {}
            Next::Ended => return,
        }
    }
}

async fn blocks<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<ASK_BYTES>,
) -> Reply<Page> {
    let page: Blocks = match abi::decode(&body) {
        Ok(page) => page,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match daemon.blocks(page).await {
        Ok(page) => Reply::Answered(page),
        Err(error) => failed(error),
    }
}

async fn block<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<ASK_BYTES>,
) -> Reply<Option<Finalized>> {
    let by: BlockRef = match abi::decode(&body) {
        Ok(by) => by,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match daemon.block(by).await {
        Ok(block) => Reply::Answered(block),
        Err(error) => failed(error),
    }
}

async fn tx<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<ASK_BYTES>,
) -> Reply<Option<Located>> {
    let ask: TxAsk = match abi::decode(&body) {
        Ok(ask) => ask,
        Err(refusal) => return Reply::Refused(refusal),
    };
    match daemon.tx(ask).await {
        Ok(tx) => Reply::Answered(tx),
        Err(error) => failed(error),
    }
}

async fn logs<E: Context>(State(daemon): State<Arc<Daemon<E>>>) -> Reply<Vec<String>> {
    Reply::Answered(daemon.logs.lines())
}

async fn admin<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<ASK_BYTES>,
) -> Reply<()> {
    let submission = match node::verify(&body, &daemon.descriptor.id()) {
        Ok(submission) => submission,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let signed_by_the_node = submission.signer == daemon.identity;
    let addressed_to_admin = submission.target == crate::wire::ADMIN;
    if !(signed_by_the_node && addressed_to_admin) {
        return Reply::Refused(Refusal::new(
            reason::INVALID_INPUT,
            "an admin verb is signed by the node's own key",
        ));
    }
    let fresh = match daemon.node.lock().await.host().fresh(&submission) {
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
            daemon.shutdown.send_replace(true);
            Reply::Answered(())
        }
        Admin::LogFilter(directives) => match daemon.logs.retune(&directives) {
            Ok(()) => Reply::Answered(()),
            Err(error) => Reply::Refused(Refusal::new(reason::INVALID_INPUT, error)),
        },
    }
}

async fn sync<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    Capped(body): Capped<ASK_BYTES>,
) -> Reply<statesync::Response> {
    let request: SyncRequest = match abi::decode(&body) {
        Ok(request) => request,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let node = daemon.node.lock().await;
    Reply::Answered(statesync::serve(&node, &daemon.anchors, request).await)
}

async fn metrics<E: Context>(State(daemon): State<Arc<Daemon<E>>>) -> String {
    daemon.context.encode()
}

async fn p2p<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let mesh = daemon.mesh;
    upgrade.on_upgrade(move |socket| crate::transport::inbound_websocket(socket, mesh))
}

async fn halo_socket<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let halo = daemon.halo.clone();
    upgrade.on_upgrade(move |socket| crate::transport::inbound_halo(socket, halo))
}

/// The UDP port a client dials for live tracks, `None` when halo bound none.
async fn live_port<E: Context>(State(daemon): State<Arc<Daemon<E>>>) -> Reply<Option<u16>> {
    Reply::Answered(daemon.halo.udp().first().map(|at| at.port()))
}

async fn live_socket<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let live = daemon.live.clone();
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

    async fn served(lane: Lane) -> String {
        let router = Router::new()
            .route("/held", get(std::future::pending::<()>))
            .layer(middleware::from_fn_with_state(lane, bounded));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{address}/held")
    }

    async fn status(url: &str) -> (u16, String) {
        let response = reqwest::get(url).await.unwrap();
        let status = response.status().as_u16();
        (status, response.text().await.unwrap())
    }

    #[tokio::test]
    async fn a_request_with_no_permit_by_its_deadline_is_answered_unavailable() {
        let lane = Lane::new(Arc::new(Semaphore::new(0)), Duration::from_millis(1));
        let (status, sentence) = status(&served(lane).await).await;
        assert_eq!(status, 503);
        assert!(sentence.contains("busy"), "{sentence}");
    }

    #[tokio::test]
    async fn a_request_unanswered_by_its_deadline_is_answered_unavailable() {
        let lane = Lane::new(Arc::new(Semaphore::new(1)), Duration::from_millis(1));
        let (status, sentence) = status(&served(lane).await).await;
        assert_eq!(status, 503);
        assert!(sentence.contains("did not answer"), "{sentence}");
    }
}
