use abi::Receipt;
use abi::{ProgramId, Root, Scan};
use borsh::{BorshDeserialize, BorshSerialize};
use host::{Layer, Tip};
use node::Preconfirm;

pub const NODE_CONTRACT: u32 = 9;
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
    pub const BLOBS: &str = "/v1/blobs";
    pub const PROGRAMS: &str = "/v1/programs";
    /// `/v1/changes/<program>[?keys]`: a websocket of each block's writes
    /// to the program (`?keys`: the keys alone), a ping for a block that
    /// wrote nothing there.
    pub const CHANGES: &str = "/v1/changes";
    pub const BLOCKS: &str = "/v1/blocks";
    pub const BLOCK: &str = "/v1/block";
    pub const TX: &str = "/v1/tx";
    pub const LOGS: &str = "/v1/logs";
    pub const ADMIN: &str = "/v1/admin";
    pub const SYNC: &str = "/v1/sync";
    pub const NETWORK: &str = "/v1/network";
    pub const P2P: &str = "/v1/p2p";
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
    pub roles: abi::Roles,
}

impl Status {
    pub fn anchor(&self) -> Tip {
        Tip {
            height: self.height,
            id: self.tip,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Submit {
    pub frame: Vec<u8>,
    pub preconfirm: Preconfirm,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Submitted {
    Pooled,
    Preconfirmed(Receipt),
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
pub enum Admin {
    Shutdown,
    LogFilter(String),
    EgressReload,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Change {
    pub height: u64,
    pub root: Root,
    pub writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Blocks {
    pub before: Option<u64>,
    pub limit: u32,
    pub most: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Page {
    pub blocks: Vec<Finalized>,
    pub listed_from: Option<u64>,
}

/// One finalized block, by height or by its id (the block digest).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum BlockRef {
    Height(u64),
    Id([u8; 32]),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BlockAsk {
    pub by: BlockRef,
    pub most: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Tx {
    pub hash: [u8; 32],
    pub signer: Vec<u8>,
    pub anchor: Tip,
    pub target: String,
    pub size: u32,
    pub payload: Option<Vec<u8>>,
    pub receipt: Option<Receipt>,
}

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

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum TxRef {
    At { height: u64, position: u32 },
    Hash([u8; 32]),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct TxAsk {
    pub tx: TxRef,
    pub most: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Located {
    pub height: u64,
    pub position: u32,
    pub time: u64,
    pub tx: Tx,
}

/// Every member of the current epoch as this node sees it (`/v1/network`).
/// `height` is this node's applied tip and no row's `signed` exceeds it,
/// so "44 behind" is `height - signed`, by this node's measure.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Network {
    pub height: u64,
    /// The epoch's members, validators and residents alike, in key order.
    pub members: Vec<PeerStatus>,
}

/// One member. `signed` is the height of the newest block this node
/// applied that the member sent a finalize vote for, as this node's
/// consensus engine heard it. A vote that lands after its block's
/// certificate reached quorum counts too; one for a block this node has
/// not applied counts once it does. The vote's sender is authenticated but
/// its signature is not checked, so a validator can misstate only its own
/// row. `None` for a member not seated as a validator this epoch (a
/// resident), and for a validator not heard since this node started.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct PeerStatus {
    pub key: Vec<u8>,
    pub signed: Option<u64>,
}
