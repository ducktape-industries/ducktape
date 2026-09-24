use std::collections::BTreeMap;

use abi::{BlobId, Entry, ProgramId, Refusal, Scan};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use futures::{Stream, StreamExt as _};
use host::{Layer, Receipt, SIGNERS};
use node::Frame;
use reqwest::StatusCode;
use statesync::{Exchange, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::wire::{BlobPut, Change, Get, Query, Range, Status, route};
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

    pub async fn submit(&self, frame: Vec<u8>) -> Result<Receipt> {
        self.post_raw(route::SUBMIT, frame).await
    }

    pub async fn submit_signed(
        &self,
        key: &PrivateKey,
        program: &str,
        payload: Vec<u8>,
    ) -> Result<Receipt> {
        let status = self.status().await?;
        let signer = key.public_key().as_ref().to_vec();
        let sequence = match self.get(Layer::Preconfirmed, SIGNERS, &signer).await? {
            Some(bytes) => abi::decode(&bytes).map_err(Error::Decode)?,
            None => 0,
        };
        let frame = Frame::sign(key, status.network.as_bytes(), sequence, program, payload);
        self.submit(frame.encode()).await
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

    pub async fn put_blob(&self, id: BlobId, framed: Vec<u8>) -> Result<()> {
        self.post(route::BLOB_PUT, &BlobPut { id, framed }).await
    }

    pub async fn missing_blobs(&self) -> Result<Vec<BlobId>> {
        self.fetch(route::BLOB_MISSING).await
    }

    pub async fn programs(&self) -> Result<BTreeMap<ProgramId, BlobId>> {
        self.fetch(route::PROGRAMS).await
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
