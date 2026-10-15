use std::collections::BTreeMap;

use abi::{BlobId, Entry, ProgramId, Refusal, Scan, reason};
use borsh::{BorshDeserialize, BorshSerialize};
use futures::{Stream, StreamExt as _};
use host::{ANCHOR_WINDOW, Layer, RECENT, Receipt, SubmissionId};
use node::{Frame, Preconfirm};
use reqwest::StatusCode;
use statesync::{Exchange, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::wire::{
    BlockAsk, BlockRef, Blocks, Change, Finalized, Get, Located, Network, Page, Query, Range,
    Status, Submit, Submitted, TxAsk, route,
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

    async fn landed(
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
                return Err(Error::Refused(Refusal::new(
                    reason::ANCHOR,
                    "no block carried the frame before its anchor aged out",
                )));
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
        self.post(route::QUERY, &Query { layer, frame }).await
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
        let encoded = frame.encode();
        let mut opening = (encoded.len() as u32).to_le_bytes().to_vec();
        opening.extend_from_slice(&encoded);
        let mut parts: Vec<Part> = vec![once(opening)];
        for upload in uploads {
            let file = tokio::fs::File::open(&upload.path).await?;
            let len = file.metadata().await?.len();
            parts.push(once(blob::header(&upload.kind, len)));
            parts.push(Box::pin(tokio_util::io::ReaderStream::new(file)));
        }
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

    pub async fn logs(&self) -> Result<Vec<String>> {
        self.fetch(route::LOGS).await
    }

    pub async fn admin(&self, frame: Vec<u8>) -> Result<()> {
        self.post_raw(route::ADMIN, frame).await
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
        StatusCode::BAD_REQUEST => {
            let refusal: Refusal = abi::decode(&body).map_err(Error::Decode)?;
            Err(Error::Refused(refusal))
        }
        status => Err(Error::Failed {
            status: status.as_u16(),
            sentence: String::from_utf8_lossy(&body).into_owned(),
        }),
    }
}

impl Exchange for Client {
    type Error = Error;

    async fn exchange(&self, request: Request) -> Result<Response> {
        self.post(route::SYNC, &request).await
    }
}
