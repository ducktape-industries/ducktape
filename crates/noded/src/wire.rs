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
    pub const BLOCKS: &str = "/v1/blocks";
    pub const BLOCK: &str = "/v1/block";
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
    /// The digest of this network's genesis block: what a client salts the
    /// network's name with to name the chain (`<network>#<salt>`).
    pub genesis: [u8; 32],
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

/// The most blocks one `/v1/blocks` page answers.
pub const MAX_BLOCKS: u32 = 100;

/// A page of finalized blocks, newest first: those below `before` (from the
/// applied tip when `None`), at most `limit` (capped at [`MAX_BLOCKS`]). A
/// page ends early where the archive holds no older block (a node that
/// joined by state sync keeps none below its anchor).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Blocks {
    pub before: Option<u64>,
    pub limit: u32,
}

/// One finalized block, by height or by its id (the block digest).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum BlockRef {
    Height(u64),
    Id([u8; 32]),
}

/// One frame a block carries, decoded. `hash` is sha256 over the frame's
/// exact bytes as the block carries them (signature included), so the same
/// signed frame has one hash wherever it is seen. A frame that does not
/// verify was not applied and is left out.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Tx {
    pub hash: [u8; 32],
    pub signer: Vec<u8>,
    pub seq: u64,
    pub target: String,
    pub payload: Vec<u8>,
}

/// A finalized block as the marshal archive keeps it. `proposer` is the
/// validator key that led the certified round, when this node holds the
/// block's finalization certificate (a block finalized only as the ancestor
/// of a later one has none of its own). There is no state root or write set
/// here: the node keeps neither per height.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Finalized {
    pub height: u64,
    pub id: [u8; 32],
    pub parent: [u8; 32],
    pub time: u64,
    pub epoch: u64,
    pub proposer: Option<Vec<u8>>,
    pub txs: Vec<Tx>,
}
