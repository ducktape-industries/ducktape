use std::sync::Arc;

use abi::{BlobId, Origin, ProgramId, Refusal, reason};
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use borsh::BorshSerialize;
use futures::StreamExt as _;
use futures::channel::mpsc;
use host::Receipt;
use statesync::Request;

use crate::wire::{Admin, BlobPut, Change, Get, Query, Range, route};
use crate::{Context, Daemon};

pub fn router<E: Context>(daemon: Arc<Daemon<E>>) -> Router {
    Router::new()
        .route(route::STATUS, get(status::<E>))
        .route(route::SUBMIT, post(submit::<E>))
        .route(route::QUERY, post(query::<E>))
        .route(route::GET, post(get_value::<E>))
        .route(route::SCAN, post(scan::<E>))
        .route(route::BLOB_GET, post(blob_get::<E>))
        .route(route::BLOB_PUT, post(blob_put::<E>))
        .route(route::BLOB_MISSING, get(blob_missing::<E>))
        .route(route::PROGRAMS, get(programs::<E>))
        .route(
            &format!("{}/{{program}}", route::CHANGES),
            get(changes::<E>),
        )
        .route(route::LOGS, get(logs::<E>))
        .route(route::ADMIN, post(admin::<E>))
        .route(route::SYNC, post(sync::<E>))
        .route(route::METRICS, get(metrics::<E>))
        .with_state(daemon)
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

async fn submit<E: Context>(State(daemon): State<Arc<Daemon<E>>>, body: Bytes) -> Reply<Receipt> {
    let mut node = daemon.node.lock().await;
    match node.submit(body.to_vec()).await {
        Ok(Ok(receipt)) => Reply::Answered(receipt),
        Ok(Err(refusal)) => Reply::Refused(refusal),
        Err(error) => failed(error),
    }
}

async fn query<E: Context>(State(daemon): State<Arc<Daemon<E>>>, body: Bytes) -> Reply<Vec<u8>> {
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
    match answer {
        Ok(Ok(bytes)) => Reply::Answered(bytes),
        Ok(Err(refusal)) => Reply::Refused(refusal),
        Err(error) => failed(error),
    }
}

async fn get_value<E: Context>(
    State(daemon): State<Arc<Daemon<E>>>,
    body: Bytes,
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
    body: Bytes,
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
    body: Bytes,
) -> Reply<Option<Vec<u8>>> {
    let id: BlobId = match abi::decode(&body) {
        Ok(id) => id,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let node = daemon.node.lock().await;
    match node.host().blob(&id) {
        Ok(framed) => Reply::Answered(framed),
        Err(host::Error::BlobUnavailable(_)) => Reply::Answered(None),
        Err(error) => failed(error),
    }
}

async fn blob_put<E: Context>(State(daemon): State<Arc<Daemon<E>>>, body: Bytes) -> Reply<()> {
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

async fn stream(mut socket: WebSocket, mut changes: mpsc::UnboundedReceiver<Change>) {
    while let Some(change) = changes.next().await {
        let sent = socket
            .send(Message::Binary(abi::encode(&change).into()))
            .await;
        if sent.is_err() {
            return;
        }
    }
}

async fn logs<E: Context>(State(daemon): State<Arc<Daemon<E>>>) -> Reply<Vec<String>> {
    Reply::Answered(daemon.logs.lines())
}

async fn admin<E: Context>(State(daemon): State<Arc<Daemon<E>>>, body: Bytes) -> Reply<()> {
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
    body: Bytes,
) -> Reply<statesync::Response> {
    let request: Request = match abi::decode(&body) {
        Ok(request) => request,
        Err(refusal) => return Reply::Refused(refusal),
    };
    let node = daemon.node.lock().await;
    Reply::Answered(statesync::serve(&node, &daemon.anchors, request).await)
}

async fn metrics<E: Context>(State(daemon): State<Arc<Daemon<E>>>) -> String {
    daemon.context.encode()
}
