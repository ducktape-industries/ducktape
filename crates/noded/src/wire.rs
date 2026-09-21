use abi::{BlobId, ProgramId, Root, Scan};
use borsh::{BorshDeserialize, BorshSerialize};
use host::Layer;

pub const NODE_CONTRACT: u32 = 1;
pub const ADMIN: &str = "$admin";

pub mod route {
    pub const STATUS: &str = "/v1/status";
    pub const SUBMIT: &str = "/v1/submit";
    pub const QUERY: &str = "/v1/query";
    pub const GET: &str = "/v1/get";
    pub const SCAN: &str = "/v1/scan";
    pub const BLOB_GET: &str = "/v1/blob/get";
    pub const BLOB_PUT: &str = "/v1/blob/put";
    pub const BLOB_MISSING: &str = "/v1/blob/missing";
    pub const PROGRAMS: &str = "/v1/programs";
    pub const CHANGES: &str = "/v1/changes";
    pub const LOGS: &str = "/v1/logs";
    pub const ADMIN: &str = "/v1/admin";
    pub const SYNC: &str = "/v1/sync";
    pub const METRICS: &str = "/metrics";
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Status {
    pub network: String,
    pub time: u64,
    pub block_time_ms: u64,
    pub epoch_length: u64,
    pub height: u64,
    pub tip: [u8; 32],
    pub root: Root,
    pub epoch: u64,
    pub identity: Vec<u8>,
    pub contract: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Query {
    pub layer: Layer,
    pub frame: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Get {
    pub layer: Layer,
    pub program: ProgramId,
    pub key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Range {
    pub layer: Layer,
    pub program: ProgramId,
    pub scan: Scan,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BlobPut {
    pub id: BlobId,
    pub framed: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Admin {
    Shutdown,
    LogFilter(String),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Change {
    pub height: u64,
    pub root: Root,
    pub writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}
