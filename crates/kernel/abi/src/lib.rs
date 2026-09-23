use borsh::{BorshDeserialize, BorshSerialize};

pub type ProgramId = String;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub struct Root(pub [u8; 32]);

impl Root {
    pub const ZERO: Root = Root([0; 32]);
}

impl core::fmt::Debug for Root {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Root({})", hex(&self.0))
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, BorshSerialize, BorshDeserialize,
)]
pub enum HashKind {
    Sha256,
    Sha1,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub enum BlobId {
    Sha256([u8; 32]),
    Sha1([u8; 20]),
}

impl BlobId {
    pub fn digest(&self) -> &[u8] {
        match self {
            BlobId::Sha256(digest) => digest,
            BlobId::Sha1(digest) => digest,
        }
    }

    pub fn kind(&self) -> HashKind {
        match self {
            BlobId::Sha256(_) => HashKind::Sha256,
            BlobId::Sha1(_) => HashKind::Sha1,
        }
    }
}

impl core::fmt::Debug for BlobId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "BlobId({:?}:{})", self.kind(), hex(self.digest()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Blob {
    pub kind: String,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct BlobHeader {
    pub kind: String,
    pub len: u64,
}

pub fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub enum Origin {
    External(Vec<u8>),
    Program(ProgramId),
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub struct ItemRef {
    pub source: ProgramId,
    pub item: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Refusal {
    pub reason: String,
    pub sentence: String,
}

impl Refusal {
    pub fn new(reason: impl Into<String>, sentence: impl Into<String>) -> Self {
        Refusal {
            reason: reason.into(),
            sentence: sentence.into(),
        }
    }
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.reason, self.sentence)
    }
}

impl std::error::Error for Refusal {}

/// A refusal's `reason` names the class of failure, which is the same as
/// naming how a caller recovers: two refusals share a token exactly when a
/// caller does the same thing about them.
pub mod reason {
    /// the host: no program by that id runs on this network.
    pub const UNKNOWN_PROGRAM: &str = "unknown_program";
    /// the host: the program faulted (a trap, the fuel or memory limit).
    pub const TRAP: &str = "trap";
    /// the host: bytes that do not decode, or an op the call's kind refuses.
    pub const PROTOCOL: &str = "protocol";
    /// the host: the frame's sequence is not the signer's next.
    pub const SEQUENCE: &str = "sequence";
    /// naming a thing that exists (id, key, path, account, sibling program).
    pub const NOT_FOUND: &str = "not_found";
    /// creating under a different id, or treating the create as done.
    pub const ALREADY_EXISTS: &str = "already_exists";
    /// re-reading and retrying: what the caller sent is behind the program.
    pub const STALE: &str = "stale";
    /// changing the thing's state first: it exists, in a state that refuses this.
    pub const WRONG_STATE: &str = "wrong_state";
    /// fixing the request: retrying it unchanged can never succeed.
    pub const INVALID_INPUT: &str = "invalid_input";
    /// sending less or removing something: a count, size or work bound is hit.
    pub const CAPACITY: &str = "capacity";
    /// waiting: the same request succeeds after a point the sentence names.
    pub const NOT_YET: &str = "not_yet";
    /// nothing: a monotonic counter cannot advance again; permanent.
    pub const EXHAUSTED: &str = "exhausted";
    /// acting as someone else: the actor may not do this to this thing.
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// configuring: the program or this deployment does not provide the op.
    pub const UNSUPPORTED: &str = "unsupported";
    /// an operator: stored state or an index failed an invariant.
    pub const CORRUPT: &str = "corrupt";
    /// an operator: a sibling program answered a shape or value this one refuses.
    pub const UNEXPECTED_REPLY: &str = "unexpected_reply";
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Outcome {
    Applied { output: Vec<u8> },
    Rejected(Refusal),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Cause {
    Direct,
    Delivery(ItemRef),
    Completion { item: ItemRef, outcome: Outcome },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Env {
    pub network: Vec<u8>,
    pub height: u64,
    pub time: u64,
    pub me: ProgramId,
    pub origin: Origin,
    pub cause: Cause,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Message {
    pub target: ProgramId,
    pub payload: Vec<u8>,
    pub reply: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Scan {
    pub lo: Vec<u8>,
    pub hi: Option<Vec<u8>>,
    pub reverse: bool,
    pub limit: Option<u64>,
}

impl Scan {
    pub fn range(lo: impl Into<Vec<u8>>, hi: Option<Vec<u8>>) -> Self {
        Scan {
            lo: lo.into(),
            hi,
            reverse: false,
            limit: None,
        }
    }

    pub fn prefix(prefix: impl AsRef<[u8]>) -> Self {
        let prefix = prefix.as_ref();
        Scan::range(prefix.to_vec(), prefix_end(prefix))
    }

    pub fn after(mut self, key: impl AsRef<[u8]>) -> Self {
        let mut lo = key.as_ref().to_vec();
        lo.push(0);
        self.lo = lo;
        self
    }

    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    pub fn admits(&self, key: &[u8]) -> bool {
        let above_lo = key >= self.lo.as_slice();
        let below_hi = self.hi.as_ref().is_none_or(|hi| key < hi.as_slice());
        above_lo && below_hi
    }
}

pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let last_bumpable = prefix.iter().rposition(|&b| b != 0xff)?;
    let mut end = prefix[..=last_bumpable].to_vec();
    end[last_bumpable] += 1;
    Some(end)
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Entry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub enum Scheme {
    Ed25519,
    Secp256k1,
    Secp256r1,
    Bls12381,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum CryptoOp {
    Sha256(Vec<u8>),
    Verify {
        scheme: Scheme,
        key: Vec<u8>,
        namespace: Vec<u8>,
        message: Vec<u8>,
        signature: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum CryptoReply {
    Digest([u8; 32]),
    Verified(bool),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum HostOp {
    Get(Vec<u8>),
    Set {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete(Vec<u8>),
    Scan(Scan),
    CommittedGet(Vec<u8>),
    CommittedScan(Scan),
    BlobPut {
        hash: HashKind,
        kind: String,
        body: Vec<u8>,
    },
    BlobGet(BlobId),
    BlobStat(BlobId),
    BlobRead {
        id: BlobId,
        offset: u64,
        len: u64,
    },
    Root(ProgramId),
    Query {
        program: ProgramId,
        request: Vec<u8>,
    },
    Emit(Message),
    Event(Vec<u8>),
    Output(Vec<u8>),
    Respond(Vec<u8>),
    Crypto(CryptoOp),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum HostReply {
    Value(Option<Vec<u8>>),
    Entries(Vec<Entry>),
    Done,
    Item(ItemRef),
    BlobId(BlobId),
    Blob(Option<Blob>),
    BlobHeader(Option<BlobHeader>),
    Root(Option<Root>),
    Query(Result<Vec<u8>, Refusal>),
    Crypto(CryptoReply),
    Refused(Refusal),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Invocation {
    pub env: Env,
    pub call: GuestCall,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum GuestCall {
    Init(Vec<u8>),
    Execute(Vec<u8>),
    Query(Vec<u8>),
}

pub type GuestReply = Result<(), Refusal>;

pub mod module_registry {
    use super::{BlobId, BorshDeserialize, BorshSerialize, ProgramId};

    pub const PROGRAM: &str = "module-registry";

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Entry {
        pub program: ProgramId,
        pub code: BlobId,
        pub params: Vec<u8>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum Query {
        At(u64),
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum Reply {
        Programs(Vec<Entry>),
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Genesis {
        pub programs: Vec<Entry>,
    }
}

pub mod valset {
    use super::{BorshDeserialize, BorshSerialize};

    pub const PROGRAM: &str = "valset";
    pub const MAX_MEMBERS: usize = 1024;

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Member {
        pub key: Vec<u8>,
        pub address: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum Query {
        Validators,
        Members,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum Reply {
        Validators(Vec<Vec<u8>>),
        Members(Vec<Member>),
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Genesis {
        pub validators: Vec<Member>,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Seating {
        pub validators: Vec<Vec<u8>>,
        pub members: Vec<Member>,
    }
}

pub mod admission {
    use super::{BorshDeserialize, BorshSerialize};

    pub const PROGRAM: &str = "admission";
    pub const INVITE_NAMESPACE: &[u8] = b"ducktape:invite";

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum Op {
        Enroll {
            address: String,
            invite: Option<Invite>,
        },
        Leave,
        Vote(Motion),
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum Motion {
        Promote { key: Vec<u8> },
        Demote { key: Vec<u8> },
        Remove { key: Vec<u8> },
        Door { open: bool },
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub enum Voted {
        Counted { votes: u64, needed: u64 },
        Enacted,
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Grant {
        pub network: Vec<u8>,
        pub nonce: Vec<u8>,
        pub expires: u64,
    }

    impl Grant {
        pub fn preimage(&self) -> Vec<u8> {
            super::encode(self)
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Invite {
        pub issuer: Vec<u8>,
        pub grant: Grant,
        pub signature: Vec<u8>,
    }
}

pub fn encode<T: BorshSerialize>(value: &T) -> Vec<u8> {
    borsh::to_vec(value).expect("borsh serialization of an in-memory value cannot fail")
}

pub fn decode<T: BorshDeserialize>(bytes: &[u8]) -> Result<T, Refusal> {
    borsh::from_slice(bytes).map_err(|e| Refusal::new(reason::PROTOCOL, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_end_is_the_first_key_past_every_extension() {
        assert_eq!(prefix_end(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(prefix_end(&[0x61, 0xff]), Some(vec![0x62]));
        assert_eq!(prefix_end(&[0xff, 0xff]), None);
        assert_eq!(prefix_end(b""), None);
        let scan = Scan::prefix(b"ab");
        assert!(scan.admits(b"ab"));
        assert!(scan.admits(b"ab\xff\xff"));
        assert!(!scan.admits(b"ac"));
        assert!(!scan.admits(b"aa"));
    }

    #[test]
    fn after_resumes_past_the_cursor() {
        let scan = Scan::prefix(b"t/").after(b"t/7");
        assert!(!scan.admits(b"t/7"));
        assert!(scan.admits(b"t/7\x00"));
        assert!(scan.admits(b"t/8"));
    }

    #[test]
    fn every_envelope_round_trips() {
        let env = Env {
            network: b"n".to_vec(),
            height: 7,
            time: 9,
            me: "a".into(),
            origin: Origin::Program("b".into()),
            cause: Cause::Completion {
                item: ItemRef {
                    source: "a".into(),
                    item: 3,
                },
                outcome: Outcome::Rejected(Refusal::new("x", "y")),
            },
        };
        let op = HostOp::Query {
            program: "b".into(),
            request: vec![1, 2],
        };
        let reply = HostReply::Query(Err(Refusal::new("r", "s")));
        let invocation = Invocation {
            env: env.clone(),
            call: GuestCall::Execute(vec![3]),
        };
        let guest_reply: GuestReply = Ok(());
        assert_eq!(decode::<Env>(&encode(&env)).unwrap(), env);
        assert_eq!(decode::<HostOp>(&encode(&op)).unwrap(), op);
        assert_eq!(decode::<HostReply>(&encode(&reply)).unwrap(), reply);
        assert_eq!(
            decode::<Invocation>(&encode(&invocation)).unwrap(),
            invocation
        );
        assert_eq!(
            decode::<GuestReply>(&encode(&guest_reply)).unwrap(),
            guest_reply
        );
        assert_eq!(decode::<Env>(&[9, 9]).unwrap_err().reason, reason::PROTOCOL);
    }
}
