use abi::Receipt;
use abi::{ProgramId, Refusal, Root, Scan};
use borsh::{BorshDeserialize, BorshSerialize};
use host::{Layer, Tip};
use node::Preconfirm;

pub const NODE_CONTRACT: u32 = 10;
pub const ADMIN: &str = "$admin";
pub const EGRESS: &str = "$egress";
pub const HOLD_HEADER: &str = "ducktape-hold";

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
    pub const BLOBS_ZSTD: &str = "/v1/blobs/zstd";
    pub const BLOBS_HELD: &str = "/v1/blobs/held";
    pub const PROGRAMS: &str = "/v1/programs";
    /// `/v1/changes/<program>[?keys]`: a websocket of each block's writes
    /// to the program (`?keys`: the keys alone), a ping carrying the block's
    /// height for a block that wrote nothing there and for each epoch's last
    /// block. A client that stops draining for a whole epoch is cut.
    pub const CHANGES: &str = "/v1/changes";
    pub const WATCH: &str = "/v1/watch";
    pub const HEADS: &str = "/v1/heads";
    pub const BLOCKS: &str = "/v1/blocks";
    pub const BLOCK: &str = "/v1/block";
    pub const TX: &str = "/v1/tx";
    pub const ACTIVITY: &str = "/v1/activity";
    pub const LOGS: &str = "/v1/logs";
    pub const HEALTH: &str = "/v1/health";
    pub const ADMIN: &str = "/v1/admin";
    pub const EGRESS_SET: &str = "/v1/egress/set";
    pub const SYNC: &str = "/v1/sync";
    pub const SYNC_HOLD: &str = "/v1/sync/hold";
    pub const SNAPSHOT: &str = "/v1/snapshot";
    pub const NETWORK: &str = "/v1/network";
    /// `POST` an `AcksAsk`: this network's settled rows for the packets the asking network
    /// names, each with its op proof at one epoch close, and that close's header unless the
    /// asker holds it.
    pub const CROSSPOND_ACKS: &str = "/v1/crosspond/acks";
    pub const HALO_SELF: &str = "/v1/halo/self";
    pub const FORWARD: &str = "/v1/forward";
    pub const FORK_AS: &str = "/v1/fork/as";
    pub const FORK_ADVANCE: &str = "/v1/fork/advance";
    pub const P2P: &str = "/v1/p2p";
    pub const METRICS: &str = "/metrics";
}

/// How a forward's far end answered its call, before any byte of the service flows.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Forwarded {
    /// The service took the connection; its bytes follow.
    Opened,
    /// The call was refused, before any connection to the service.
    Refused(Refusal),
}

impl Forwarded {
    pub fn of<T>(outcome: &std::result::Result<T, Refusal>) -> Forwarded {
        match outcome {
            Ok(_) => Forwarded::Opened,
            Err(refusal) => Forwarded::Refused(refusal.clone()),
        }
    }
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
pub struct Play {
    pub body: node::Body,
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
    pub ask: Ask,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Ask {
    Signed(Vec<u8>),
    Unsigned { target: ProgramId, payload: Vec<u8> },
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
pub struct EgressSet {
    pub text: String,
    pub dry: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct EgressPlan {
    pub served: u64,
    pub added: u64,
    pub removed: u64,
    pub changed: u64,
    pub reach: Vec<(String, Reached)>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Reached {
    Answered(u16),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Change {
    pub height: u64,
    pub root: Root,
    pub writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Head {
    pub height: u64,
    pub id: [u8; 32],
    pub time: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Watch {
    Ask { id: u32, frame: Vec<u8> },
    Drop { id: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Pushed {
    Answer {
        id: u32,
        height: u64,
        outcome: Outcome,
    },
    Block {
        height: u64,
        root: Root,
        writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        answers: Vec<(u32, Outcome)>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Outcome {
    Answered(Vec<u8>),
    Refused(Refusal),
    Moved,
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
pub struct AcksAsk {
    pub from: abi::NetworkId,
    pub seqs: Vec<u64>,
    pub held: Option<u64>,
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

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum HaloSelf {
    Unregistered,
    Dialing { at: String },
    Reached { at: String },
    Unreachable { at: String, reason: String },
}

pub mod activity {
    use abi::role::identity::AccountNumber;
    use abi::{ProgramId, Refusal};
    use borsh::{BorshDeserialize, BorshSerialize};

    #[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct PageRequest {
        pub after: Option<Vec<u8>>,
        pub limit: Option<u64>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct PageResponse<T> {
        pub height: u64,
        pub items: Vec<T>,
        pub next: Option<Vec<u8>>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Frame {
        pub height: u64,
        pub position: u32,
        pub time: u64,
        pub signer: Vec<u8>,
        pub target: ProgramId,
        pub applied: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Used {
        pub account: AccountNumber,
        pub program: ProgramId,
        pub frames: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Indexed {
        pub from: u64,
        pub height: u64,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum Query {
        Acted {
            account: AccountNumber,
            page: PageRequest,
        },
        Signed {
            key: Vec<u8>,
            page: PageRequest,
        },
        Used {
            page: PageRequest,
        },
        Frames {
            page: PageRequest,
        },
        Indexed,
        Refusal {
            height: u64,
            position: u32,
        },
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum Reply {
        Frames(PageResponse<Frame>),
        Used(PageResponse<Used>),
        Indexed(Indexed),
        Refusal(Option<Refusal>),
    }
}

pub mod snapshot {
    use borsh::{BorshDeserialize, BorshSerialize};

    #[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Ask {
        pub resume: Option<Resume>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Resume {
        pub height: u64,
        pub path: String,
        pub offset: u64,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Part {
        pub path: String,
        pub len: u64,
        pub from: u64,
    }
}

#[cfg(test)]
mod tests {
    use abi::Refusal;

    use super::{Outcome, Pushed, Watch};

    #[test]
    fn the_watch_wire_keeps_the_tags_it_was_ruled_with() {
        let ask = Watch::Ask {
            id: 1,
            frame: vec![9],
        };
        assert_eq!(abi::encode(&ask), [0, 1, 0, 0, 0, 1, 0, 0, 0, 9]);
        assert_eq!(abi::encode(&Watch::Drop { id: 2 }), [1, 2, 0, 0, 0]);
        assert_eq!(abi::encode(&Outcome::Answered(vec![7])), [0, 1, 0, 0, 0, 7]);
        let refused = Outcome::Refused(Refusal::new("r", "s"));
        assert_eq!(abi::encode(&refused)[0], 1);
        assert_eq!(abi::encode(&Outcome::Moved), [2]);
        let answer = Pushed::Answer {
            id: 3,
            height: 4,
            outcome: Outcome::Moved,
        };
        assert_eq!(
            abi::encode(&answer),
            [0, 3, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 2]
        );
        let block = Pushed::Block {
            height: 5,
            root: abi::Root([0; 32]),
            writes: Vec::new(),
            answers: Vec::new(),
        };
        assert_eq!(abi::encode(&block)[0], 1);
    }
}
