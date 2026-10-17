use std::collections::BTreeMap;

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

/// Who a frame acts as, resolved by the host once per frame through the
/// identity role: an account (the one a signer's key holds, or the account
/// of the program that sent a message), or the chain itself.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub enum Principal {
    Account(role::identity::AccountNumber),
    System,
}

/// Each role's program, by the role's name: bound at founding, and a role
/// outside `role::REQUIRED` moves on the validators' vote (`Motion::Bind`)
/// to any admitted program, or goes. The kernel calls some rows; others
/// (egress, halo, scheduler, any name a program looks up) it never does.
/// Every frame's env carries the table, so a program asks a role by its
/// binding, never by a name it assumes.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Roles(BTreeMap<String, ProgramId>);

impl Roles {
    pub fn get(&self, role: &str) -> Option<&ProgramId> {
        self.0.get(role)
    }

    pub fn bound(&self, role: &str) -> Result<&ProgramId, Refusal> {
        self.get(role).ok_or_else(|| {
            Refusal::new(
                reason::UNSUPPORTED,
                format!("this network binds no program to the {role} role"),
            )
        })
    }

    pub fn bind(&mut self, role: impl Into<String>, program: impl Into<ProgramId>) {
        self.0.insert(role.into(), program.into());
    }

    pub fn unbind(&mut self, role: &str) {
        self.0.remove(role);
    }

    pub fn unfilled(&self) -> Option<&'static str> {
        role::REQUIRED
            .iter()
            .copied()
            .find(|role| !self.0.contains_key(*role))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ProgramId)> {
        self.0
            .iter()
            .map(|(role, program)| (role.as_str(), program))
    }
}

impl<R: Into<String>, P: Into<ProgramId>> FromIterator<(R, P)> for Roles {
    fn from_iter<I: IntoIterator<Item = (R, P)>>(rows: I) -> Self {
        Roles(
            rows.into_iter()
                .map(|(role, program)| (role.into(), program.into()))
                .collect(),
        )
    }
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
    pub const ANCHOR: &str = "anchor";
    pub const DUPLICATE: &str = "duplicate";
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
pub struct Receipt {
    pub program: ProgramId,
    pub outcome: Outcome,
    pub events: Vec<Vec<u8>>,
    pub nested: Vec<Receipt>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Cause {
    Direct,
    Message(ItemRef),
    Completion { item: ItemRef, outcome: Outcome },
    Call { ticket: role::awaits::Ticket },
    Scheduled { entry: role::scheduler::EntryId },
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Env {
    pub network: Vec<u8>,
    pub height: u64,
    pub time: u64,
    pub me: ProgramId,
    pub origin: Origin,
    /// `None` for a query, and for a frame whose key or program holds no
    /// account.
    pub sender: Option<Principal>,
    pub roles: Roles,
    pub cause: Cause,
}

/// What a program emits: `target` runs `payload` in the same frame once
/// the emitting handler returns, and with `reply` its outcome comes back
/// as a [`Cause::Completion`].
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Message {
    pub target: ProgramId,
    pub payload: Vec<u8>,
    pub reply: bool,
    pub acting: Option<role::identity::AccountNumber>,
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
    Await {
        target: ProgramId,
        payload: Vec<u8>,
        acting: Option<role::identity::AccountNumber>,
    },
    Defer,
    Resume {
        frame: role::awaits::FrameId,
        ticket: role::awaits::Ticket,
        answer: role::awaits::Answer,
    },
    /// Bind `program` to `role` in the network's roles table, or unbind the
    /// role when `program` is `None`: the validators role's alone, and never
    /// a role in `role::REQUIRED`.
    Bind {
        role: String,
        program: Option<ProgramId>,
    },
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
    Ticket(role::awaits::Ticket),
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Invocation {
    pub env: Env,
    pub call: ProgramCall,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum ProgramCall {
    Init(Vec<u8>),
    Execute(Vec<u8>),
    Query(Vec<u8>),
    Hook(hook::Record),
    Resume {
        ticket: role::awaits::Ticket,
        answer: role::awaits::Answer,
    },
}

pub mod hook {
    use crate::{BorshDeserialize, BorshSerialize, ProgramId, role::identity::AccountNumber};

    #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Record {
        pub height: u64,
        pub position: u32,
        pub signer: Vec<u8>,
        pub account: Option<AccountNumber>,
        pub target: ProgramId,
        pub applied: bool,
    }
}

pub type ProgramReply = Result<(), Refusal>;

pub mod role {
    pub const PROGRAMS: &str = "programs";
    pub const VALIDATORS: &str = "validators";
    pub const IDENTITY: &str = "identity";
    pub const BLOBS: &str = "blobs";
    pub const AWAITS: &str = "awaits";
    pub const EGRESS: &str = "egress";
    pub const SCHEDULER: &str = "scheduler";
    pub const HALO: &str = "halo";
    /// The roles a network binds for its life: the kernel calls the first
    /// five, and every node reads halo's lane table to serve its lanes, so
    /// no network founds without them. The others may be bound, rebound or
    /// unbound by the validators' vote.
    pub const REQUIRED: &[&str] = &[PROGRAMS, VALIDATORS, IDENTITY, BLOBS, AWAITS, HALO];

    /// Which code runs at a height.
    pub mod programs {
        use crate::{BlobId, BorshDeserialize, BorshSerialize, ProgramId};

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Entry {
            pub program: ProgramId,
            pub code: BlobId,
            pub params: Vec<u8>,
        }

        /// A view with no program behind it: its name on the rail and the blob
        /// that is the view itself.
        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct View {
            pub name: ProgramId,
            pub view: BlobId,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        #[borsh(use_discriminant = true)]
        #[repr(u8)]
        pub enum Query {
            At(u64) = 0,
            Hooks(u64) = 5,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        #[borsh(use_discriminant = true)]
        #[repr(u8)]
        pub enum Reply {
            Programs(Vec<Entry>) = 0,
            Hooks(Vec<ProgramId>) = 5,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Genesis {
            pub programs: Vec<Entry>,
            pub views: Vec<View>,
            pub hooks: Vec<ProgramId>,
        }
    }

    /// The consensus set.
    pub mod validators {
        use crate::{BorshDeserialize, BorshSerialize};

        /// The most members a network's cap can name.
        pub const MAX_MEMBERS: u32 = 128;

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Member {
            pub key: Vec<u8>,
            pub address: Option<String>,
            pub halo: String,
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
            /// How many members (validators and residents) the network ever holds
            /// at once: 1..=MAX_MEMBERS, fixed at founding.
            pub member_cap: u32,
        }

        pub const INVITE_NAMESPACE: &[u8] = b"ducktape:invite";

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Op {
            Enroll {
                address: Option<String>,
                halo: String,
                invite: Option<Invite>,
            },
            Leave,
            Vote(Motion),
            SetHalo {
                halo: String,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Motion {
            Promote {
                key: Vec<u8>,
            },
            Demote {
                key: Vec<u8>,
            },
            Remove {
                key: Vec<u8>,
            },
            Door {
                open: bool,
            },
            /// The validators' vote on an op of the program bound to `role`:
            /// on enactment the op runs on that program as the vote.
            Role {
                role: String,
                op: Vec<u8>,
            },
            /// The validators' vote on the roles table: bind `program` to
            /// `role`, or unbind the role when `program` is `None`. A role in
            /// `REQUIRED` is refused: its program is fixed.
            Bind {
                role: String,
                program: Option<crate::ProgramId>,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Voted {
            Counted { votes: u64, needed: u64 },
            Enacted,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Grant {
            pub network: Vec<u8>,
            pub nonce: [u8; 16],
            pub expires: u64,
        }

        impl Grant {
            pub fn preimage(&self) -> Vec<u8> {
                crate::encode(self)
            }
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Invite {
            pub issuer: Vec<u8>,
            pub grant: Grant,
            pub signature: Vec<u8>,
        }

        pub fn registered_halo(halo: &str) -> Result<(), crate::Refusal> {
            let refused = |why: &str| {
                crate::Refusal::new(
                    crate::reason::INVALID_INPUT,
                    format!("halo: `{halo}` {why}"),
                )
            };
            let Some((host, port)) = halo.rsplit_once(':') else {
                return Err(refused("is not host:port"));
            };
            let ported = port.parse::<u16>().is_ok_and(|port| port != 0);
            if host.is_empty() || !ported {
                return Err(refused("is not host:port"));
            }
            let bare = host
                .strip_prefix('[')
                .and_then(|inner| inner.strip_suffix(']'))
                .unwrap_or(host);
            let name = bare.trim_end_matches('.').to_ascii_lowercase();
            let named_localhost = name == "localhost" || name.ends_with(".localhost");
            let bind_only = bare
                .parse::<core::net::IpAddr>()
                .map(|ip| ip.to_canonical())
                .is_ok_and(|ip| ip.is_unspecified() || ip.is_loopback());
            if named_localhost || bind_only {
                return Err(refused(
                    "is an unspecified or loopback host: register the host:port other members dial",
                ));
            }
            Ok(())
        }
    }

    /// Who holds a key or runs as a program (the account a frame acts as),
    /// and how each account reads to the programs and views that name it.
    pub mod identity {
        use crate::{BorshDeserialize, BorshSerialize, ProgramId};

        pub type AccountNumber = u64;

        /// What an account is, in one field, so no account is two things
        /// at once. A `User` is a person who holds their own keys. A
        /// `Program` account is managed by the user its `manager` names:
        /// it holds the keys that user gives it, acts only while its
        /// `standing` is `Active`, and its `script` runs its actions: the
        /// script `calls` one program and hands it `config`, text that
        /// program alone gives a meaning. A `System` account is a
        /// program's own account and holds no keys.
        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Kind {
            User,
            Program {
                manager: AccountNumber,
                category: Category,
                standing: Standing,
                script: Option<Script>,
            },
            System(ProgramId),
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Script {
            pub calls: ProgramId,
            pub config: String,
        }

        /// What a manager declares a program account to be. The enum only
        /// grows at its end.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Category {
            Agent,
        }

        /// Whether a program account acts. Its manager alone changes it;
        /// `Revoked` is final.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Standing {
            Active,
            Suspended,
            Revoked,
        }

        /// An account as others show it: its name and what it is.
        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Profile {
            pub number: AccountNumber,
            pub name: String,
            pub kind: Kind,
        }

        /// The one write the kernel makes: as it admits a program, with the
        /// `System` origin, it gives the program its account. Registering a
        /// program that has one changes nothing.
        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Op {
            RegisterProgram { program: ProgramId },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Query {
            /// The kernel's: the account a frame signed by this key acts
            /// as, `None` for a key that holds none. Refused while that
            /// account does not act (a program account whose [`Standing`] is
            /// not `Active`). The host rejects a frame whose key is
            /// refused.
            Account(Vec<u8>),
            /// The kernel's: the account of a program, answered as
            /// `Account`.
            OfProgram(ProgramId),
            /// One account's profile, `None` for a number no account has.
            Profile(AccountNumber),
            /// Every account's profile, ascending by number from past
            /// `after`, at most `limit` of them (the program may cap it).
            Profiles {
                after: Option<AccountNumber>,
                limit: u32,
            },
            Runs {
                program: ProgramId,
                account: AccountNumber,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Reply {
            Account(Option<AccountNumber>),
            Profile(Option<Profile>),
            /// `next` is the `after` of the following page; `None` at the end.
            Profiles {
                profiles: Vec<Profile>,
                next: Option<AccountNumber>,
            },
        }
    }

    pub mod awaits {
        use crate::{BorshDeserialize, BorshSerialize, ProgramId, Refusal, reason};

        pub type Ticket = u64;
        pub type FrameId = u64;
        pub type Answer = Result<Vec<u8>, Refusal>;

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Call {
            pub ticket: Ticket,
            pub target: ProgramId,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Op {
            Park {
                frame: FrameId,
                program: ProgramId,
                calls: Vec<Call>,
            },
            Resolve {
                ticket: Ticket,
                answer: Answer,
            },
            Close {
                frame: FrameId,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct TicketRow {
            pub ticket: Ticket,
            pub frame: FrameId,
            pub caller: ProgramId,
            pub target: ProgramId,
            pub opened: u64,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Query {
            Ticket {
                ticket: Ticket,
            },
            Owed {
                target: ProgramId,
                after: Option<Ticket>,
                limit: u32,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Reply {
            Ticket(Option<TicketRow>),
            Tickets(Vec<TicketRow>),
        }

        pub fn cancelled(program: &str) -> Refusal {
            Refusal::new(
                reason::WRONG_STATE,
                format!("{program} changed code while its frame awaited"),
            )
        }
    }

    /// The lanes halo carries between members: each named lane's wire id, the
    /// class its streams are served at most, whether it is served and whether
    /// it is pinned. Only the validators' vote changes the table, and no vote
    /// pauses or closes a pinned lane.
    pub mod halo {
        use crate::{BorshDeserialize, BorshSerialize};

        /// The lane byte on the wire: a stream hello's first byte, a
        /// datagram's first byte.
        pub type LaneId = u8;
        pub const MAX_NAME: usize = 64;

        /// The priority a halo stream is served at: Critical outranks
        /// Member outranks Client.
        #[derive(
            Clone,
            Copy,
            Debug,
            PartialEq,
            Eq,
            Hash,
            PartialOrd,
            Ord,
            BorshSerialize,
            BorshDeserialize,
        )]
        pub enum Class {
            Critical,
            Member,
            Client,
        }

        impl Class {
            pub const COUNT: usize = 3;
            pub const ALL: [Class; Class::COUNT] = [Class::Critical, Class::Member, Class::Client];

            pub fn rank(self) -> usize {
                self as usize
            }
        }

        impl From<Class> for u8 {
            fn from(class: Class) -> u8 {
                class as u8
            }
        }

        impl TryFrom<u8> for Class {
            type Error = u8;

            fn try_from(byte: u8) -> Result<Class, u8> {
                Class::ALL.get(usize::from(byte)).copied().ok_or(byte)
            }
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Lane {
            pub name: String,
            pub id: LaneId,
            pub cap: Class,
            pub enabled: bool,
            /// Set at founding and never changed: a pinned lane is served for
            /// the network's life, so a lane a node needs to apply blocks
            /// cannot be voted away from under it.
            pub pinned: bool,
            /// The block that opened it.
            pub since: u64,
        }

        /// Every op is the validators' vote (`Motion::Role`); a signed frame
        /// is refused.
        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Op {
            /// Lists `name` at the next free id, enabled and not pinned.
            Open { name: String, cap: Class },
            /// Changes the cap and whether the lane is served; the id stays.
            /// A pinned lane refuses `enabled: false` (`invalid_input`).
            Set {
                name: String,
                cap: Class,
                enabled: bool,
            },
            /// Unlists `name`; its id is never handed out again. A pinned lane
            /// refuses it (`invalid_input`).
            Close { name: String },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Query {
            /// Every listed lane, in id order.
            Lanes,
            Lane {
                name: String,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Reply {
            Lanes(Vec<Lane>),
            Lane(Option<Lane>),
        }

        /// A lane the network is founded with.
        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Founded {
            pub name: String,
            pub cap: Class,
            pub pinned: bool,
        }

        /// The founding lanes, listed in order from id 0.
        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Genesis {
            pub lanes: Vec<Founded>,
        }
    }

    pub mod egress {
        use crate::role::identity::AccountNumber;
        use crate::role::scheduler::EntryId;
        use crate::{BlobId, BorshDeserialize, BorshSerialize, ProgramId};

        pub const DEFAULT_WAIT: u64 = 64;

        pub const HTTP: &str = "http";
        pub const RELAY: &str = "relay";
        pub const RESERVED: [&str; 2] = [HTTP, RELAY];

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Op {
            Register {
                service: String,
            },
            Assign {
                service: String,
                nodes: Vec<Vec<u8>>,
            },
            Serve {
                services: Vec<String>,
            },
            Request {
                request: Request,
                delivery: Delivery,
                wait: Option<u64>,
            },
            Answer {
                id: u64,
                answer: Answer,
            },
            Event(Event),
            EndStream {
                id: u64,
                events: u64,
                end: StreamEnd,
            },
            Expire {
                id: u64,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Request {
            Relay {
                note: Vec<u8>,
            },
            Http {
                service: String,
                head: http::Head,
                body: Vec<BlobId>,
            },
        }

        impl Request {
            pub fn service(&self) -> &str {
                match self {
                    Request::Relay { .. } => RELAY,
                    Request::Http { service, .. } => service,
                }
            }

            pub fn parts(&self) -> &[BlobId] {
                match self {
                    Request::Relay { .. } => &[],
                    Request::Http { body, .. } => body,
                }
            }
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Answer {
            Output(Vec<u8>),
            Failed(String),
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Delivery {
            Whole,
            Events { deliver: Vec<u8> },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Event {
            pub id: u64,
            pub seq: u64,
            pub data: Vec<u8>,
        }

        impl Event {
            pub fn payload(&self, deliver: &[u8]) -> Vec<u8> {
                let mut payload = deliver.to_vec();
                payload.extend(crate::encode(self));
                payload
            }
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum StreamEnd {
            Complete {
                status: u16,
                headers: Vec<(String, String)>,
            },
            Cut {
                reason: String,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct ServiceRow {
            pub name: String,
            pub owner: AccountNumber,
            pub nodes: Vec<Vec<u8>>,
            pub registered: u64,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct RequestRow {
            pub id: u64,
            pub requester: ProgramId,
            pub request: Request,
            pub delivery: Delivery,
            pub node: Vec<u8>,
            pub made: u64,
            pub expiry: EntryId,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct NodeRow {
            pub key: Vec<u8>,
            pub services: Vec<String>,
            pub since: u64,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        #[borsh(use_discriminant = true)]
        #[repr(u8)]
        pub enum Query {
            Assigned { key: Vec<u8> } = 0,
            Node { key: Vec<u8> } = 1,
            Serving { service: String } = 2,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        #[borsh(use_discriminant = true)]
        #[repr(u8)]
        pub enum Reply {
            Requests(Vec<RequestRow>) = 0,
            Node(Option<NodeRow>) = 1,
            Serving(Vec<Vec<u8>>) = 2,
        }

        pub mod http {
            use crate::{BorshDeserialize, BorshSerialize, Refusal, reason};

            pub const FRAMING: [&str; 4] =
                ["host", "content-length", "transfer-encoding", "connection"];

            pub const NODE_PREFIX: &str = "ducktape-";

            #[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
            pub enum Method {
                Get,
                Post,
                Put,
                Patch,
                Delete,
            }

            impl Method {
                pub fn as_str(self) -> &'static str {
                    match self {
                        Method::Get => "GET",
                        Method::Post => "POST",
                        Method::Put => "PUT",
                        Method::Patch => "PATCH",
                        Method::Delete => "DELETE",
                    }
                }
            }

            #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
            pub struct Head {
                pub method: Method,
                pub target: String,
                pub headers: Vec<(String, String)>,
            }

            #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
            pub struct Response {
                pub status: u16,
                pub headers: Vec<(String, String)>,
                pub body: Vec<u8>,
            }

            fn printable(text: &str) -> bool {
                text.bytes()
                    .all(|byte| byte.is_ascii_graphic() && byte != b'\\')
            }

            fn refused(sentence: impl Into<String>) -> Refusal {
                Refusal::new(reason::INVALID_INPUT, sentence)
            }

            impl Head {
                pub fn check_url(&self) -> Result<(), Refusal> {
                    let after_scheme = self
                        .target
                        .strip_prefix("https://")
                        .or_else(|| self.target.strip_prefix("http://"));
                    let host = after_scheme
                        .and_then(|rest| rest.split(['/', '?', '#']).next())
                        .unwrap_or_default();
                    let fits = !host.is_empty() && printable(&self.target);
                    if !fits {
                        return Err(refused(
                            "an http target is http:// or https://, then a host, all printable \
                             ASCII",
                        ));
                    }
                    check_headers(&self.headers)
                }

                pub fn check_path(&self) -> Result<(), Refusal> {
                    let path = self.target.split(['?', '#']).next().unwrap_or_default();
                    let climbs = path.split('/').any(|segment| {
                        let decoded = segment.to_ascii_lowercase().replace("%2e", ".");
                        decoded == "." || decoded == ".."
                    });
                    let fits = self.target.starts_with('/') && printable(&self.target) && !climbs;
                    if !fits {
                        return Err(refused(
                            "a service's target is a path: it starts with /, has no . or .. \
                             segment, and is printable ASCII",
                        ));
                    }
                    check_headers(&self.headers)
                }
            }

            pub fn check_headers(headers: &[(String, String)]) -> Result<(), Refusal> {
                for (name, value) in headers {
                    let named = !name.is_empty()
                        && name.bytes().all(|byte| {
                            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                        });
                    if !named {
                        return Err(refused(format!(
                            "header name {name:?} is lowercase letters, digits and -"
                        )));
                    }
                    let reserved =
                        FRAMING.contains(&name.as_str()) || name.starts_with(NODE_PREFIX);
                    if reserved {
                        return Err(refused(format!(
                            "{name} is the node's to set, never a request's"
                        )));
                    }
                    let printable = value
                        .bytes()
                        .all(|byte| byte == b'\t' || (b' '..=b'~').contains(&byte));
                    if !printable {
                        return Err(refused(format!("header {name} is printable ASCII")));
                    }
                }
                Ok(())
            }
        }
    }

    pub mod scheduler {
        use crate::{BorshDeserialize, BorshSerialize, ProgramId, Receipt};

        pub type EntryId = u64;

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Op {
            Schedule {
                height: u64,
                target: ProgramId,
                payload: Vec<u8>,
            },
            Cancel {
                entry: EntryId,
            },
            Ran {
                entry: EntryId,
                receipt: Receipt,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub struct Entry {
            pub id: EntryId,
            pub scheduler: ProgramId,
            pub height: u64,
            pub target: ProgramId,
            pub payload: Vec<u8>,
            pub receipt: Option<Receipt>,
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Query {
            Due {
                height: u64,
                after: Option<EntryId>,
                limit: Option<u32>,
            },
            Entry {
                id: EntryId,
            },
        }

        #[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        pub enum Reply {
            Due {
                entries: Vec<Entry>,
                next: Option<EntryId>,
            },
            Entry(Option<Entry>),
        }
    }
}

/// Who may publish and subscribe to a room's live tracks through a node:
/// the program that owns the room decides in its own code and writes the
/// decision as state under this table; the node reads it and enforces it.
/// No row for an account and no `*` row means the account may do nothing in
/// the room.
pub mod live {
    use crate::{BorshDeserialize, BorshSerialize, role::identity::AccountNumber};

    /// The reserved table a program writes live grants in: key
    /// `live/<room>/<who>`, `who` the account number in decimal or `*` for
    /// every account that acts on this chain; value a borsh [`Grant`].
    pub const TABLE: &str = "live";

    /// A track name, in bytes: UTF-8, `/`-separated segments, none empty.
    /// Here beside the grant grammar so a program names the bound it
    /// publishes under instead of copying the number.
    pub const MAX_TRACK_NAME: usize = 128;
    /// A namespace, in bytes: `<chain>/<program>/<room>` as the host stamps it.
    pub const MAX_NAMESPACE: usize = 192;

    /// `live/<room>/<who>`: the key of one grant row.
    pub fn key(room: &str, who: Option<AccountNumber>) -> Vec<u8> {
        let who = match who {
            Some(account) => account.to_string(),
            None => "*".to_owned(),
        };
        format!("{TABLE}/{room}/{who}").into_bytes()
    }

    /// What one account, or every account, may do in one room. A pattern is
    /// a track name exactly, a prefix ending in `/*` (`presence/*`), or `*`;
    /// `{account}` in it is the asking account's number, so one `*` row binds
    /// each name to its own account (`echo/{account}`).
    #[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
    pub struct Grant {
        /// Track names this account may publish; empty = may not publish.
        pub publish: Vec<String>,
        /// Track names this account may subscribe to and watch; empty = may
        /// not subscribe.
        pub subscribe: Vec<String>,
    }

    impl Grant {
        /// Publish and subscribe to every track of the room.
        pub fn all() -> Grant {
            Grant {
                publish: vec!["*".into()],
                subscribe: vec!["*".into()],
            }
        }

        /// Whether `account` may publish the track `name`.
        pub fn publishes(&self, name: &str, account: AccountNumber) -> bool {
            covers(&self.publish, name, account)
        }

        /// Whether `account` may subscribe to the track `name`, and hears it when it watches
        /// the room.
        pub fn subscribes(&self, name: &str, account: AccountNumber) -> bool {
            covers(&self.subscribe, name, account)
        }

        /// Whether the holder may watch the room: any subscribe pattern is enough, so a watch
        /// turns on no track name and no account.
        pub fn watches(&self) -> bool {
            !self.subscribe.is_empty()
        }

        /// The subscribe patterns `account` hears the room's announces through, as they read
        /// for it; `None` when it may not watch.
        pub fn watch_patterns(&self, account: AccountNumber) -> Option<Vec<String>> {
            self.watches().then(|| {
                self.subscribe
                    .iter()
                    .map(|pattern| for_account(pattern, account))
                    .collect()
            })
        }
    }

    /// What a grant pattern says for the account that asks: [`ACCOUNT`] in it is the account's
    /// number in decimal, so one row binds each name to its own account (`echo/{account}`).
    pub const ACCOUNT: &str = "{account}";

    /// `pattern` as it reads for `account`: each [`ACCOUNT`] written as its number.
    pub fn for_account(pattern: &str, account: AccountNumber) -> String {
        pattern.replace(ACCOUNT, &account.to_string())
    }

    /// A grant pattern, as it reads for the account that asks ([`for_account`]), against a
    /// track name: `*` matches every name, `<prefix>/*` every name under the prefix, anything
    /// else exactly.
    pub fn pattern_covers(pattern: &str, name: &str) -> bool {
        if pattern == "*" {
            return true;
        }
        match pattern.strip_suffix("/*") {
            Some(prefix) => name
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/')),
            None => pattern == name,
        }
    }

    fn covers(patterns: &[String], name: &str, account: AccountNumber) -> bool {
        patterns
            .iter()
            .any(|pattern| pattern_covers(&for_account(pattern, account), name))
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
    fn a_registered_halo_address_names_a_host_other_members_dial() {
        use role::validators::registered_halo;
        for dialed in [
            "a.example.org:9100",
            "203.0.113.7:9100",
            "[2001:db8::1]:9100",
            "localhost-like.example:1",
        ] {
            assert_eq!(registered_halo(dialed), Ok(()), "{dialed}");
        }
        for unregistrable in [
            "0.0.0.0:9100",
            "[::]:9100",
            ":::9100",
            "127.0.0.1:9100",
            "127.1.2.3:9100",
            "[::1]:9100",
            "[::ffff:127.0.0.1]:9100",
            "localhost:9100",
            "LOCALHOST.:9100",
            "node.localhost:9100",
            "a.example.org",
            "a.example.org:0",
            ":9100",
        ] {
            let refusal = registered_halo(unregistrable).unwrap_err();
            assert_eq!(refusal.reason, reason::INVALID_INPUT, "{unregistrable}");
            assert!(refusal.sentence.starts_with("halo: "), "{refusal:?}");
        }
    }

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
    fn the_programs_role_keeps_at_on_tag_zero_and_asks_hooks_on_tag_five() {
        use role::programs::{Query, Reply};
        let at = Query::At(7);
        let hooks = Query::Hooks(7);
        assert_eq!(encode(&at), [&[0u8][..], &7u64.to_le_bytes()].concat());
        assert_eq!(encode(&hooks), [&[5u8][..], &7u64.to_le_bytes()].concat());
        assert_eq!(decode::<Query>(&encode(&hooks)).unwrap(), hooks);
        let listed = Reply::Hooks(vec!["activity".into()]);
        assert_eq!(encode(&listed)[0], 5);
        assert_eq!(decode::<Reply>(&encode(&listed)).unwrap(), listed);
        assert_eq!(encode(&Reply::Programs(Vec::new())), [0u8, 0, 0, 0, 0]);
        assert_eq!(
            decode::<Query>(&[1, 0, 0, 0, 0, 0, 0, 0, 0])
                .unwrap_err()
                .reason,
            reason::PROTOCOL
        );
    }

    #[test]
    fn a_grant_row_is_keyed_by_room_and_account_or_star() {
        assert_eq!(live::key("lobby", Some(42)), b"live/lobby/42".to_vec());
        assert_eq!(live::key("lobby", None), b"live/lobby/*".to_vec());
        let all = live::Grant::all();
        assert_eq!(decode::<live::Grant>(&encode(&all)).unwrap(), all);
        assert_eq!(
            live::Grant::default(),
            live::Grant {
                publish: vec![],
                subscribe: vec![]
            }
        );
    }

    #[test]
    fn a_pattern_is_everything_a_prefix_or_one_name() {
        use live::pattern_covers;
        assert!(pattern_covers("*", "anything/at/all"));
        assert!(pattern_covers("presence/*", "presence/42"));
        assert!(pattern_covers("presence/*", "presence/a/b"));
        assert!(!pattern_covers("presence/*", "presence"));
        assert!(!pattern_covers("presence/*", "presencex/42"));
        assert!(pattern_covers("doc", "doc"));
        assert!(!pattern_covers("doc", "doc/1"));
    }

    #[test]
    fn an_account_pattern_is_the_asking_accounts_own() {
        use live::{for_account, pattern_covers};
        let mine = for_account("echo/{account}", 42);
        assert_eq!(mine, "echo/42");
        assert!(pattern_covers(&mine, "echo/42"));
        assert!(!pattern_covers(&mine, "echo/43"));
        let under = for_account("ask/{account}/*", 42);
        assert!(pattern_covers(&under, "ask/42/7/0b"));
        assert!(!pattern_covers(&under, "ask/43/42/0b"));
        assert!(!pattern_covers(
            &for_account("ask/{account}/*", 4),
            "ask/42/7"
        ));
        assert_eq!(for_account("doc", 42), "doc");
    }

    #[test]
    fn a_grant_decides_each_side_by_its_own_patterns_as_they_read_for_the_account() {
        let room = live::Grant {
            publish: vec!["echo/{account}".into(), "board/{account}/*".into()],
            subscribe: vec!["lobby".into(), "inbox/{account}".into()],
        };
        assert!(room.publishes("echo/42", 42));
        assert!(room.publishes("board/42/0b", 42));
        assert!(!room.publishes("echo/42", 13));
        assert!(!room.publishes("lobby", 42));
        assert!(room.subscribes("lobby", 13));
        assert!(room.subscribes("inbox/13", 13));
        assert!(!room.subscribes("inbox/42", 13));
        assert!(!room.subscribes("echo/13", 13));
        assert!(!live::Grant::default().subscribes("lobby", 13));
        assert!(live::Grant::all().publishes("any/name", 13));
    }

    #[test]
    fn a_class_survives_its_byte() {
        use role::halo::Class;
        for class in Class::ALL {
            assert_eq!(Class::try_from(u8::from(class)), Ok(class));
        }
        assert_eq!(Class::try_from(3), Err(3));
    }

    #[test]
    fn the_egress_role_asks_on_fixed_tags_a_program_extends_after() {
        use role::egress::{Query, Reply};
        assert_eq!(encode(&Query::Assigned { key: vec![7] })[0], 0);
        assert_eq!(encode(&Query::Node { key: vec![7] })[0], 1);
        let serving = Query::Serving {
            service: "s".into(),
        };
        assert_eq!(encode(&serving)[0], 2);
        assert_eq!(encode(&Reply::Requests(Vec::new())), [0, 0, 0, 0, 0]);
        assert_eq!(encode(&Reply::Node(None)), [1, 0]);
        assert_eq!(encode(&Reply::Serving(Vec::new())), [2, 0, 0, 0, 0]);
    }

    #[test]
    fn a_streamed_event_reaches_its_requester_as_the_requesters_own_op() {
        use role::egress::Event;
        #[derive(Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
        enum Requester {
            Other,
            Streamed(Event),
        }
        let event = Event {
            id: 7,
            seq: 3,
            data: b"event: delta\ndata: {}\n".to_vec(),
        };
        let streamed_tag = encode(&Requester::Other)[0] + 1;
        let delivered = event.payload(&[streamed_tag]);
        assert_eq!(
            decode::<Requester>(&delivered).unwrap(),
            Requester::Streamed(event)
        );
    }

    #[test]
    fn an_egress_head_names_a_url_or_a_path_and_leaves_framing_to_the_node() {
        use role::egress::http::{Head, Method};
        let head = |headers: &[(&str, &str)], target: &str| Head {
            method: Method::Post,
            target: target.into(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        };
        let url = "https://api.example.org/v1/messages";
        for fits in [url, "http://127.0.0.1:8080/x?y=1"] {
            assert_eq!(head(&[], fits).check_url(), Ok(()), "{fits}");
        }
        for unfit in [
            "/v1/messages",
            "ftp://host/",
            "https:///path",
            "https://host/a b",
        ] {
            let refusal = head(&[], unfit).check_url().unwrap_err();
            assert_eq!(refusal.reason, reason::INVALID_INPUT, "{unfit}");
        }
        for fits in ["/", "/v1/messages", "/v1/models/m?beta=true", "/a..b/c."] {
            assert_eq!(head(&[], fits).check_path(), Ok(()), "{fits}");
        }
        for unfit in [
            "v1/messages",
            url,
            "/../admin",
            "/v1/./x",
            "/v1/%2E%2e/x",
            "/v1/..",
            "/v1\\..\\x",
            "/a b",
        ] {
            assert!(head(&[], unfit).check_path().is_err(), "{unfit}");
        }
        for name in ["authorization", "x-api-key", "cookie"] {
            assert_eq!(head(&[(name, "x")], "/").check_path(), Ok(()), "{name}");
        }
        for name in [
            "host",
            "content-length",
            "connection",
            "ducktape-program",
            "X-Up",
        ] {
            assert!(head(&[(name, "x")], "/").check_path().is_err(), "{name}");
        }
    }

    #[test]
    fn a_role_table_names_the_first_kernel_role_it_lacks() {
        let full: Roles = role::REQUIRED.iter().map(|role| (*role, *role)).collect();
        assert_eq!(full.unfilled(), None);
        assert_eq!(full.get(role::EGRESS), None);
        assert_eq!(
            full.bound(role::EGRESS).map_err(|refusal| refusal.reason),
            Err(reason::UNSUPPORTED.to_owned())
        );
        for lacking in role::REQUIRED {
            let short: Roles = role::REQUIRED
                .iter()
                .filter(|role| *role != lacking)
                .map(|role| (*role, *role))
                .collect();
            assert_eq!(short.unfilled(), Some(*lacking));
        }
    }

    #[test]
    fn a_role_table_carries_a_row_no_reader_names() {
        let mut roles: Roles = role::REQUIRED.iter().map(|role| (*role, *role)).collect();
        roles.bind("later", "later");
        let read: Roles = decode(&encode(&roles)).unwrap();
        assert_eq!(read.get("later").map(String::as_str), Some("later"));
        assert_eq!(read, roles);
    }

    #[test]
    fn a_scheduled_run_and_its_entry_round_trip() {
        use role::scheduler::{Entry, Op, Query, Reply};
        let receipt = Receipt {
            program: "t".into(),
            outcome: Outcome::Rejected(Refusal::new("x", "y")),
            events: vec![vec![1]],
            nested: vec![Receipt {
                program: "n".into(),
                outcome: Outcome::Applied { output: vec![2] },
                events: Vec::new(),
                nested: Vec::new(),
            }],
        };
        let entry = Entry {
            id: 3,
            scheduler: "s".into(),
            height: 9,
            target: "t".into(),
            payload: vec![4],
            receipt: Some(receipt.clone()),
        };
        let ran = Op::Ran { entry: 3, receipt };
        let due = Query::Due {
            height: 9,
            after: Some(2),
            limit: None,
        };
        let page = Reply::Due {
            entries: vec![entry],
            next: None,
        };
        let cause = Cause::Scheduled { entry: 3 };
        assert_eq!(decode::<Op>(&encode(&ran)).unwrap(), ran);
        assert_eq!(decode::<Query>(&encode(&due)).unwrap(), due);
        assert_eq!(decode::<Reply>(&encode(&page)).unwrap(), page);
        assert_eq!(decode::<Cause>(&encode(&cause)).unwrap(), cause);
    }

    #[test]
    fn every_envelope_round_trips() {
        let env = Env {
            network: b"n".to_vec(),
            height: 7,
            time: 9,
            me: "a".into(),
            origin: Origin::Program("b".into()),
            sender: Some(Principal::Account(4)),
            roles: Roles::from_iter([
                (role::PROGRAMS, "r"),
                (role::VALIDATORS, "v"),
                (role::IDENTITY, "i"),
                (role::BLOBS, "b"),
                (role::AWAITS, "a"),
                (role::EGRESS, "e"),
                (role::SCHEDULER, "s"),
                (role::HALO, "h"),
            ]),
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
            call: ProgramCall::Hook(hook::Record {
                height: 7,
                position: 2,
                signer: vec![1],
                account: Some(4),
                target: "chat".into(),
                applied: false,
            }),
        };
        let program_reply: ProgramReply = Ok(());
        assert_eq!(decode::<Env>(&encode(&env)).unwrap(), env);
        assert_eq!(decode::<HostOp>(&encode(&op)).unwrap(), op);
        assert_eq!(decode::<HostReply>(&encode(&reply)).unwrap(), reply);
        assert_eq!(
            decode::<Invocation>(&encode(&invocation)).unwrap(),
            invocation
        );
        assert_eq!(
            decode::<ProgramReply>(&encode(&program_reply)).unwrap(),
            program_reply
        );
        assert_eq!(decode::<Env>(&[9, 9]).unwrap_err().reason, reason::PROTOCOL);
    }
}
