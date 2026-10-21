use std::collections::BTreeMap;

use abi::Receipt;
use abi::{BlobId, Entry, ProgramId, Refusal, Scan, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use futures::{Stream, StreamExt as _};
use host::{ANCHOR_WINDOW, Layer, RECENT, SubmissionId};
use node::{Frame, Preconfirm};
use reqwest::StatusCode;
use statesync::{Ending, Exchange, Failure, Fault, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::wire::{
    Ask, BlockAsk, BlockRef, Blocks, Change, Finalized, Get, Located, Network, Page, Query, Range,
    Status, Submit, Submitted, TxAsk, activity, route,
};
use crate::{Error, Result};

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    pub fn new(base: impl Into<String>) -> Client {
        Client {
            http: reqwest::Client::new(),
            base: base.into(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn status(&self) -> Result<Status> {
        self.fetch(route::STATUS).await
    }

    /// The current epoch's members and, for each validator, the newest
    /// block this node applied that it heard the validator's finalize vote
    /// for (`wire::PeerStatus`).
    pub async fn network(&self) -> Result<Network> {
        self.fetch(route::NETWORK).await
    }

    /// The UDP port the node's relay takes live tracks at, `None` when it bound none.
    pub async fn live_port(&self) -> Result<Option<u16>> {
        self.fetch(live_wire::PORT_ROUTE).await
    }

    pub async fn submit(&self, frame: Vec<u8>, preconfirm: Preconfirm) -> Result<Submitted> {
        self.post(route::SUBMIT, &Submit { frame, preconfirm })
            .await
    }

    pub async fn land(&self, frame: &Frame) -> Result<(u64, Receipt)> {
        let blocks = self.changes(RECENT).await?;
        self.submit(frame.encode(), Preconfirm::Off).await?;
        self.landed(frame, blocks).await
    }

    pub async fn land_blobs(&self, frame: &Frame, uploads: Vec<Upload>) -> Result<(u64, Receipt)> {
        let blocks = self.changes(RECENT).await?;
        self.put_blobs(frame, uploads).await?;
        self.landed(frame, blocks).await
    }

    pub async fn land_framed(&self, frame: &Frame, framed: Vec<Vec<u8>>) -> Result<(u64, Receipt)> {
        let blocks = self.changes(RECENT).await?;
        self.put_framed(frame, framed).await?;
        self.landed(frame, blocks).await
    }

    pub async fn landed(
        &self,
        frame: &Frame,
        mut blocks: impl Stream<Item = Result<Change>> + Unpin,
    ) -> Result<(u64, Receipt)> {
        let submission = frame.body.submission();
        let id = submission.id();
        let last = submission.anchor.height + ANCHOR_WINDOW;
        while let Some(change) = blocks.next().await {
            let height = change?.height;
            if let Some(receipt) = self.carried(height, &id).await? {
                return Ok((height, receipt));
            }
            let aged_out = height >= last;
            if aged_out {
                return Err(Error::Unlanded);
            }
        }
        Err(Error::Closed)
    }

    async fn carried(&self, height: u64, id: &SubmissionId) -> Result<Option<Receipt>> {
        let ask = BlockAsk {
            by: BlockRef::Height(height),
            most: 0,
        };
        let Some(block) = self.block(ask).await? else {
            return Ok(None);
        };
        let tx = block.txs.into_iter().find(|tx| &tx.hash == id);
        Ok(tx.and_then(|tx| tx.receipt))
    }

    pub async fn query(&self, layer: Layer, frame: Vec<u8>) -> Result<Vec<u8>> {
        let ask = Ask::Signed(frame);
        self.post(route::QUERY, &Query { layer, ask }).await
    }

    pub async fn query_unsigned(
        &self,
        layer: Layer,
        target: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let ask = Ask::Unsigned {
            target: target.to_owned(),
            payload,
        };
        self.post(route::QUERY, &Query { layer, ask }).await
    }

    pub async fn get(&self, layer: Layer, program: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let get = Get {
            layer,
            program: program.to_owned(),
            key: key.to_vec(),
        };
        self.post(route::GET, &get).await
    }

    pub async fn scan(&self, layer: Layer, program: &str, scan: Scan) -> Result<Vec<Entry>> {
        let range = Range {
            layer,
            program: program.to_owned(),
            scan,
        };
        self.post(route::SCAN, &range).await
    }

    pub async fn blob(&self, id: BlobId) -> Result<Option<Vec<u8>>> {
        self.post(route::BLOB_GET, &id).await
    }

    pub async fn put_blobs(&self, frame: &Frame, uploads: Vec<Upload>) -> Result<Submitted> {
        let mut parts: Vec<Part> = Vec::new();
        for upload in uploads {
            let file = tokio::fs::File::open(&upload.path).await?;
            let len = file.metadata().await?.len();
            parts.push(once(blob::header(&upload.kind, len)));
            parts.push(Box::pin(tokio_util::io::ReaderStream::new(file)));
        }
        self.put_parts(frame, parts).await
    }

    pub async fn put_framed(&self, frame: &Frame, framed: Vec<Vec<u8>>) -> Result<Submitted> {
        let parts = framed.into_iter().map(once).collect();
        self.put_parts(frame, parts).await
    }

    async fn put_parts(&self, frame: &Frame, parts: Vec<Part>) -> Result<Submitted> {
        let encoded = frame.encode();
        let mut opening = (encoded.len() as u32).to_le_bytes().to_vec();
        opening.extend_from_slice(&encoded);
        let parts = std::iter::once(once(opening)).chain(parts);
        let body = reqwest::Body::wrap_stream(futures::stream::iter(parts).flatten());
        let response = self
            .http
            .post(self.url(route::BLOB_PUT))
            .body(body)
            .send()
            .await?;
        answered(response).await
    }

    pub async fn held(&self, role: &str, bundle: u64) -> Result<u64> {
        let key = blob::contract::bundle_key(bundle);
        let mut changes = self.changes(role).await?;
        let recorded = self.get(Layer::Confirmed, role, &key).await?;
        if let Some(since) = blob::contract::held_since(recorded.as_deref()) {
            return Ok(since);
        }
        while let Some(change) = changes.next().await {
            let written = change?
                .writes
                .into_iter()
                .find_map(|(written, value)| (written == key).then_some(value));
            if let Some(since) = blob::contract::held_since(written.flatten().as_deref()) {
                return Ok(since);
            }
        }
        Err(Error::Closed)
    }

    pub async fn blobs(&self, ids: &[BlobId]) -> Result<Vec<Option<Vec<u8>>>> {
        self.post(route::BLOBS, &ids).await
    }

    pub async fn missing_blobs(&self) -> Result<Vec<BlobId>> {
        self.fetch(route::BLOB_MISSING).await
    }

    pub async fn programs(&self) -> Result<BTreeMap<ProgramId, BlobId>> {
        self.fetch(route::PROGRAMS).await
    }

    pub async fn blocks(&self, page: Blocks) -> Result<Page> {
        self.post(route::BLOCKS, &page).await
    }

    pub async fn block(&self, ask: BlockAsk) -> Result<Option<Finalized>> {
        self.post(route::BLOCK, &ask).await
    }

    pub async fn tx(&self, ask: TxAsk) -> Result<Option<Located>> {
        self.post(route::TX, &ask).await
    }

    pub async fn activity(&self, query: &activity::Query) -> Result<activity::Reply> {
        self.post(route::ACTIVITY, query).await
    }

    pub async fn snapshot(&self, into: &std::path::Path) -> Result<statesync::Manifest> {
        let resume = crate::snapshot::resumed(into).await?;
        let ask = crate::wire::snapshot::Ask { resume };
        let response = self
            .http
            .post(self.url(route::SNAPSHOT))
            .body(abi::encode(&ask))
            .send()
            .await?;
        let status = response.status();
        let answered = status == StatusCode::OK;
        if !answered {
            let body = response.bytes().await?;
            return Err(unanswered(status, &body));
        }
        let answer = response
            .bytes_stream()
            .map(|read| read.map_err(Error::from));
        crate::snapshot::fetched(answer, into).await
    }

    pub async fn logs(&self) -> Result<Vec<String>> {
        self.fetch(route::LOGS).await
    }

    pub async fn admin(&self, frame: Vec<u8>) -> Result<()> {
        self.post_raw(route::ADMIN, frame).await
    }

    pub async fn egress_set(&self, frame: Vec<u8>) -> Result<crate::wire::EgressPlan> {
        self.post_raw(route::EGRESS_SET, frame).await
    }

    pub async fn metrics(&self) -> Result<String> {
        let response = self.http.get(self.url(route::METRICS)).send().await?;
        Ok(response.error_for_status()?.text().await?)
    }

    pub async fn changes(
        &self,
        program: &str,
    ) -> Result<impl Stream<Item = Result<Change>> + Unpin> {
        let url = format!(
            "{}{}/{program}",
            self.base.replacen("http", "ws", 1),
            route::CHANGES
        );
        let unbounded = WebSocketConfig {
            max_message_size: None,
            max_frame_size: None,
            ..WebSocketConfig::default()
        };
        let (socket, _) =
            tokio_tungstenite::connect_async_with_config(url, Some(unbounded), false).await?;
        Ok(socket.filter_map(|message| {
            let change = match message {
                Ok(Message::Binary(bytes)) => Some(abi::decode(&bytes).map_err(Error::Decode)),
                Ok(_) => None,
                Err(error) => Some(Err(Error::Ws(Box::new(error)))),
            };
            futures::future::ready(change)
        }))
    }

    fn url(&self, route: &str) -> String {
        format!("{}{route}", self.base)
    }

    async fn fetch<T: BorshDeserialize>(&self, route: &str) -> Result<T> {
        let response = self.http.get(self.url(route)).send().await?;
        answered(response).await
    }

    async fn post<B: BorshSerialize, T: BorshDeserialize>(
        &self,
        route: &str,
        body: &B,
    ) -> Result<T> {
        self.post_raw(route, abi::encode(body)).await
    }

    async fn post_raw<T: BorshDeserialize>(&self, route: &str, body: Vec<u8>) -> Result<T> {
        let response = self.http.post(self.url(route)).body(body).send().await?;
        answered(response).await
    }
}

pub struct Upload {
    pub kind: String,
    pub path: std::path::PathBuf,
}

type Part = std::pin::Pin<Box<dyn Stream<Item = std::io::Result<bytes::Bytes>> + Send>>;

fn once(bytes: Vec<u8>) -> Part {
    Box::pin(futures::stream::once(futures::future::ready(Ok(
        bytes::Bytes::from(bytes),
    ))))
}

async fn answered<T: BorshDeserialize>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let body = response.bytes().await?;
    match status {
        StatusCode::OK => abi::decode(&body).map_err(Error::Decode),
        status => Err(unanswered(status, &body)),
    }
}

fn unanswered(status: StatusCode, body: &[u8]) -> Error {
    match status {
        StatusCode::BAD_REQUEST => match abi::decode::<Refusal>(body) {
            Ok(refusal) => Error::Refused(refusal),
            Err(refusal) => Error::Decode(refusal),
        },
        status => Error::Failed {
            status: status.as_u16(),
            sentence: String::from_utf8_lossy(body).into_owned(),
        },
    }
}

impl Exchange for Client {
    type Error = Error;

    async fn exchange(&self, request: Request) -> Result<Response> {
        self.post(route::SYNC, &request).await.map_err(without_url)
    }

    async fn hold(&self) -> Result<(Response, Ending)> {
        held(self).await.map_err(without_url)
    }

    fn stream_blobs(&self, ids: Vec<BlobId>) -> impl Stream<Item = Result<Option<Vec<u8>>>> + Send {
        let answers =
            futures::stream::once(blob_answers(self.clone(), ids)).flat_map(
                |opened| match opened {
                    Ok(answers) => answers.left_stream(),
                    Err(error) => {
                        futures::stream::once(futures::future::ready(Err(error))).right_stream()
                    }
                },
            );
        answers.map(|read| read.map_err(without_url))
    }
}

fn without_url(error: Error) -> Error {
    match error {
        Error::Http(error) => Error::Http(error.without_url()),
        error => error,
    }
}

async fn held(client: &Client) -> Result<(Response, Ending)> {
    let response = client
        .http
        .post(client.url(route::SYNC_HOLD))
        .send()
        .await?;
    let status = response.status();
    let answered = status == StatusCode::OK;
    if !answered {
        let body = response.bytes().await?;
        return Err(unanswered(status, &body));
    }
    let mut body: Body = Box::pin(response.bytes_stream());
    let mut read = Vec::new();
    loop {
        if let Some(head) = framed_head(&read)? {
            let holding: Ending =
                Box::pin(async move { while let Some(Ok(_)) = body.next().await {} });
            return Ok((head, holding));
        }
        match body.next().await {
            Some(Ok(bytes)) => read.extend_from_slice(&bytes),
            Some(Err(error)) => return Err(error.into()),
            None => {
                return Err(Error::Decode(Refusal::new(
                    reason::PROTOCOL,
                    "the hold's answer ended before its head was whole",
                )));
            }
        }
    }
}

fn framed_head(read: &[u8]) -> Result<Option<Response>> {
    let Some(len) = read.first_chunk::<4>() else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(*len) as usize;
    let Some(head) = read[4..].get(..len) else {
        return Ok(None);
    };
    abi::decode(head).map(Some).map_err(Error::Decode)
}

type Body = std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>;

async fn blob_answers(
    client: Client,
    ids: Vec<BlobId>,
) -> Result<impl Stream<Item = Result<Option<Vec<u8>>>> + Send> {
    let response = client
        .http
        .post(client.url(route::BLOBS))
        .body(abi::encode(&ids))
        .send()
        .await?;
    let status = response.status();
    let answered = status == StatusCode::OK;
    if !answered {
        let body = response.bytes().await?;
        return Err(unanswered(status, &body));
    }
    let reading = Reading {
        body: Box::pin(response.bytes_stream()),
        answer: Answer::default(),
    };
    Ok(futures::stream::unfold(Some(reading), next_blob))
}

struct Reading {
    body: Body,
    answer: Answer,
}

async fn next_blob(reading: Option<Reading>) -> Option<(Result<Option<Vec<u8>>>, Option<Reading>)> {
    let mut reading = reading?;
    loop {
        match reading.answer.take() {
            Ok(Taken::Blob(framed)) => return Some((Ok(framed), Some(reading))),
            Ok(Taken::Ended) => return None,
            Ok(Taken::Wanting) => {}
            Err(error) => return Some((Err(error), None)),
        }
        match reading.body.next().await {
            Some(Ok(bytes)) => reading.answer.held.extend_from_slice(&bytes),
            Some(Err(error)) => return Some((Err(error.into()), None)),
            None => return Some((Err(Error::Short), None)),
        }
    }
}

#[derive(Default)]
struct Answer {
    held: Vec<u8>,
    left: Option<u32>,
}

enum Taken {
    Blob(Option<Vec<u8>>),
    Wanting,
    Ended,
}

impl Answer {
    fn take(&mut self) -> Result<Taken> {
        let Some(left) = self.left else {
            return self.counted();
        };
        if left == 0 {
            return Ok(Taken::Ended);
        }
        let Some((framed, used)) = item(&self.held)? else {
            return Ok(Taken::Wanting);
        };
        self.held.drain(..used);
        self.left = Some(left - 1);
        Ok(Taken::Blob(framed))
    }

    fn counted(&mut self) -> Result<Taken> {
        let Some(count) = self.held.first_chunk::<4>() else {
            return Ok(Taken::Wanting);
        };
        self.left = Some(u32::from_le_bytes(*count));
        self.held.drain(..4);
        self.take()
    }
}

type Item = (Option<Vec<u8>>, usize);

fn item(held: &[u8]) -> Result<Option<Item>> {
    let Some((&tag, rest)) = held.split_first() else {
        return Ok(None);
    };
    match tag {
        0 => Ok(Some((None, 1))),
        1 => {
            let Some(len) = rest.first_chunk::<4>() else {
                return Ok(None);
            };
            let len = u32::from_le_bytes(*len) as usize;
            let Some(framed) = rest[4..].get(..len) else {
                return Ok(None);
            };
            Ok(Some((Some(framed.to_vec()), 5 + len)))
        }
        tag => Err(Error::Decode(Refusal::new(
            reason::PROTOCOL,
            format!("a blob answer's item opens with {tag}, not an option's tag"),
        ))),
    }
}

impl Fault for Error {
    fn failure(&self) -> Failure {
        match self {
            Error::Http(error) => http_failure(error),
            Error::Failed { status, .. } => status_failure(*status),
            Error::Short => Failure::Transient("source_dropped"),
            Error::Io(_)
            | Error::Founding(_)
            | Error::Settings { .. }
            | Error::NoListen
            | Error::NoHalo
            | Error::Cadence(_)
            | Error::PastPermits { .. }
            | Error::Hex(_)
            | Error::Address(_)
            | Error::Tls(_)
            | Error::Corrupt(_)
            | Error::Node(_)
            | Error::Host(_)
            | Error::Blob(_)
            | Error::Sync(_)
            | Error::Archives(_)
            | Error::Store(_)
            | Error::Marshal(_)
            | Error::Lane(_)
            | Error::Membership(_)
            | Error::Ws(_)
            | Error::Decode(_)
            | Error::Refused(_)
            | Error::Closed
            | Error::Unlanded
            | Error::Stopped
            | Error::Offloaded(_)
            | Error::Egress(_)
            | Error::Unrelayed
            | Error::MachineIsIdentity
            | Error::Unrolled(_) => Failure::Definite,
        }
    }
}

fn http_failure(error: &reqwest::Error) -> Failure {
    let untrusted = causes(error).any(|cause| cause.is::<rustls::Error>());
    if untrusted {
        return Failure::Definite;
    }
    if error.is_connect() {
        return Failure::Transient("source_unreachable");
    }
    let dropped = error.is_request() || error.is_body() || error.is_decode();
    if dropped {
        return Failure::Transient("source_dropped");
    }
    Failure::Definite
}

fn causes<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> impl Iterator<Item = &'a (dyn std::error::Error + 'static)> {
    std::iter::successors(Some(error), |&error| {
        let wrapped = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref);
        match wrapped {
            Some(inner) => Some(inner as &(dyn std::error::Error + 'static)),
            None => error.source(),
        }
    })
}

fn status_failure(status: u16) -> Failure {
    let Ok(status) = StatusCode::from_u16(status) else {
        return Failure::Definite;
    };
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Failure::Transient("source_busy");
    }
    if status.is_server_error() {
        return Failure::Transient("source_failed");
    }
    Failure::Definite
}
