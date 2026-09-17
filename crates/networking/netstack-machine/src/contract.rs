//! The machine's frozen surface: every input ([`Event`]), every output
//! ([`Effect`]), the correlation ids that tie them together, and the
//! configuration the host resolves once and hands in whole. This module is
//! what the netstack arc's scenario traces replay against and what the
//! `ducktape:netstack` wasm world will export — treat every shape change
//! here as a contract change.

use std::net::SocketAddr;
use std::time::Duration;

use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
use commonware_cryptography::ed25519;
use nat_traversal::NodeKey;
use wireguard::effect::PeerTunnelConfig;
use wireguard::wire_schema::{socket_addr, vec_socket_addr};
use wireguard::{
    Endpoint, MeshVersion, PortPolicy, UpgradeError, ValidatorIdentity, X25519PublicKey,
};

use crate::wire::{key, keys, option_key, result_socket_addr};

/// How long each coordinator interaction (reflexive discovery, lookup) may
/// take before the host resolver moves on. Declared HERE because the
/// machine's rendezvous backoff is derived from it — the protocol's timing
/// envelope is part of the contract, and the host resolver honors it.
pub const COORD_STEP_TIMEOUT: Duration = Duration::from_secs(3);
/// One punch exchange attempt; retried [`PUNCH_TRIES`] times before the
/// resolution fails (the peer then rides its advertised endpoint — the
/// coordinator is rendezvous-only, there is no relay to fall back to).
pub const PUNCH_STEP_TIMEOUT: Duration = Duration::from_secs(1);
pub const PUNCH_TRIES: usize = 3;

/// How a peer's WireGuard endpoint was resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub enum Resolution {
    /// Dial the advertised endpoint as-is (public or already-reachable
    /// address; also the no-coordinator dev path).
    Advertised,
    /// Hole-punch succeeded: dial the peer's punched reflexive.
    Punched(
        #[borsh(schema(with_funcs(
            declaration = "socket_addr::declaration",
            definitions = "socket_addr::definitions"
        )))]
        SocketAddr,
    ),
}

/// Correlates a started operation (an [`Effect`] that will produce an
/// outcome) with the [`Event`] that carries the outcome back. Minted by the
/// machine, monotonic within its life.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
)]
pub struct ReqId(pub u64);

/// Correlates a host command that awaits a reply (an invite install, the
/// coordinated-invite bootstrap) with the [`Effect::ReplyInstall`] /
/// [`Effect::ReplyIntro`] that answers it. Minted by the HOST — the machine
/// only threads it through, never interprets it.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
)]
pub struct CmdToken(pub u64);

/// Everything the machine decides over, resolved ONCE by the host and handed
/// in whole: public key material, policy, and the advertised endpoints. No
/// paths and no private keys of any kind — the filesystem is the host's
/// domain, the WireGuard PRIVATE key never enters the machine (interface
/// pushes carry peer sets; the host assembles the full interface config),
/// and the identity signs through an [`wireguard::IdentitySigner`] handed
/// to [`crate::Machine::new`] beside this config. Plain data, so it crosses
/// the guest boundary as one wire value.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub struct MachineConfig {
    /// The chain id — doubles as the advertisement namespace and the ULA
    /// derivation input, exactly as it does for the commonware mesh.
    pub chain_id: String,
    /// The node's X25519 WireGuard PUBLIC key (the host owns the private
    /// half): what records, handshake messages, and peers' installs carry.
    pub wireguard_public: X25519PublicKey,
    /// The node's own advertised WireGuard UDP endpoint — `None` for an
    /// endpoint-less (NAT'd) node: it advertises no address, installs every
    /// peer FROM the records, and initiates; peers install it without an
    /// endpoint and WireGuard roams to its authenticated initiation.
    pub wireguard_advertised: Option<Endpoint>,
    /// The node's own advertised control-mesh endpoint.
    pub control_endpoint: Endpoint,
    /// Rendezvous coordinators (from `Resolved.coordinated`), possibly none:
    /// with an empty list every peer resolves to its advertised endpoint.
    #[borsh(schema(with_funcs(
        declaration = "vec_socket_addr::declaration",
        definitions = "vec_socket_addr::definitions"
    )))]
    pub coordinators: Vec<SocketAddr>,
    /// The endpoint policy advertisements and handshakes validate against.
    pub port_policy: PortPolicy,
    /// Whether the host persists the applied mesh: `true` makes the machine
    /// emit [`Effect::Persist`] snapshots (and expect restore bytes on the
    /// boot retarget); `false` runs the plane on live assembly alone.
    pub persist: bool,
    /// A transport identity whose DELIVERIES are admitted even though it is
    /// no plane participant: the mesh's derived lobby key, which a parked
    /// standby connects under while its own key is still untracked. Purely
    /// an ingress allowance — every message still authenticates by its
    /// owner's content signature, and standby-directed replies route back
    /// over whichever transport identity delivered the standby's record.
    #[borsh(
        serialize_with = "option_key::serialize",
        deserialize_with = "option_key::deserialize",
        schema(with_funcs(
            declaration = "option_key::declaration",
            definitions = "option_key::definitions"
        ))
    )]
    pub gossip_ingress: Option<ed25519::PublicKey>,
}

/// The step boundary every backend implements: the native [`crate::Machine`]
/// and a wasm guest behind a host embedding. The executor drives one through
/// this trait and never learns which it holds.
pub trait NetstackMachine {
    /// Step one event, stamped with the caller's unix-millisecond clock, and
    /// return the effects to perform IN ORDER.
    fn step(&mut self, event: Event, now_ms: u64) -> Result<Vec<Effect>, StepError>;

    /// The machine's whole state as one wire value ([`crate::wire`]'s
    /// `snapshot` root): what a backend swap carries to the machine that
    /// takes over. Taken between steps only. `&mut` because a guest's call
    /// runs inside its store, under its step budget.
    fn snapshot(&mut self) -> Result<Vec<u8>, StepError>;
}

impl<M: NetstackMachine + ?Sized> NetstackMachine for Box<M> {
    fn step(&mut self, event: Event, now_ms: u64) -> Result<Vec<Effect>, StepError> {
        (**self).step(event, now_ms)
    }

    fn snapshot(&mut self) -> Result<Vec<u8>, StepError> {
        (**self).snapshot()
    }
}

/// Why a step produced no effects.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// A protocol invariant breach (a commitment that failed to compute):
    /// the plane cannot continue past it, whichever backend raised it.
    #[error("protocol: {0:?}")]
    Protocol(#[from] UpgradeError),
    /// The backend itself failed to run the step — a guest trap, an
    /// exhausted fuel budget, an undecodable wire value. The machine's state
    /// is unknown from here on; the executor stops the plane. It never
    /// selects a different implementation after a fault.
    #[error("backend fault: {0}")]
    Fault(String),
}

/// A valset cutover (or boot) the machine must retarget to.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub struct MeshEpochEvent {
    pub epoch: u64,
    /// The epoch's consensus members' ed25519 public keys, this node
    /// included. Order is irrelevant — every derived commitment sorts.
    #[borsh(
        serialize_with = "keys::serialize",
        deserialize_with = "keys::deserialize",
        schema(with_funcs(declaration = "keys::declaration", definitions = "keys::definitions"))
    )]
    pub members: Vec<ed25519::PublicKey>,
    /// The epoch's STANDBY identities (the valset resident tier): registered
    /// keys the pre-warm layer tunnels toward ahead of their activation.
    /// Never part of the epoch's `ActiveValidatorSet` — a standby that never
    /// shows up costs the epoch nothing.
    #[borsh(
        serialize_with = "keys::serialize",
        deserialize_with = "keys::deserialize",
        schema(with_funcs(declaration = "keys::declaration", definitions = "keys::definitions"))
    )]
    pub standbys: Vec<ed25519::PublicKey>,
    /// The consensus view at the cutover; the freshness clock for expiries.
    pub current_view: u64,
}

/// Machine -> node observability and mesh-send surface. (The host executor
/// wraps [`Effect::MeshSend`] into [`ReachabilityEvent::Send`] so the node's
/// event pump keeps its single channel.)
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub enum ReachabilityEvent {
    /// Send `bytes` to `to` on the reachability channel.
    Send {
        #[borsh(
            serialize_with = "key::serialize",
            deserialize_with = "key::deserialize",
            schema(with_funcs(
                declaration = "key::declaration",
                definitions = "key::definitions"
            ))
        )]
        to: ed25519::PublicKey,
        bytes: Vec<u8>,
    },
    /// Every member's signed advertisement verified into one `MeshView`.
    MeshReady { epoch: u64, version: MeshVersion },
    /// The epoch's tunnel config went through the effect: one interface,
    /// `peers` peer relationships.
    TunnelsApplied {
        epoch: u64,
        interface: String,
        peers: usize,
    },
    /// A peer could not be brought up (handshake refused, resolution
    /// failed) or sent traffic it had no business sending. The mesh keeps
    /// going without it; the node surfaces the warning.
    PeerFailed {
        #[borsh(
            serialize_with = "key::serialize",
            deserialize_with = "key::deserialize",
            schema(with_funcs(
                declaration = "key::declaration",
                definitions = "key::definitions"
            ))
        )]
        peer: ed25519::PublicKey,
        reason: String,
    },
    /// The epoch as a whole failed (mesh verification, effect rejection).
    /// Previous tunnels stay as they were; the next cutover retries.
    EpochFailed { epoch: u64, reason: String },
    /// The persisted mesh re-applied at boot: `peers` tunnels from epoch
    /// `epoch`'s remembered records, endpoints freshly coordinator-resolved.
    /// Purely a gossip carrier — the boot epoch's own assembly replaces it.
    MeshRestored {
        epoch: u64,
        interface: String,
        peers: usize,
    },
    /// The boot re-apply could not run (unreadable state file, effect
    /// rejection). The plane continues on live assembly alone — exactly the
    /// pre-persistence behavior.
    RestoreFailed { reason: String },
    /// The epoch applied but its mesh could not be persisted; a cold restart
    /// would restore the PREVIOUS persisted epoch (or nothing).
    PersistFailed { reason: String },
    /// Record-derived pre-warm tunnels merged onto the interface: `peers`
    /// counts the standby<->member relationships now installed alongside
    /// (on a member) or ahead of (on a standby) the phase-A mesh.
    StandbyTunnelsApplied {
        epoch: u64,
        interface: String,
        peers: usize,
    },
    /// Every peer's advert agreed on one mesh version this node's fresh
    /// record is not part of: this node re-assembled mid-epoch (a restart)
    /// after the peers locked. Their mesh is adopted as the applied base —
    /// tunnels from their signed records, endpoints freshly resolved — and
    /// this node's record keeps re-offering until every peer re-tunnels it;
    /// the next cutover folds everything back into one verified mesh.
    MeshAdopted {
        epoch: u64,
        version: MeshVersion,
        peers: usize,
    },
    /// A member signed a new record after this epoch locked its mesh (a
    /// restart, an address rebind): its tunnel was re-pointed in place as a
    /// layer over the applied base, leaving the rest of the mesh untouched.
    PeerReadvertised {
        #[borsh(
            serialize_with = "key::serialize",
            deserialize_with = "key::deserialize",
            schema(with_funcs(
                declaration = "key::declaration",
                definitions = "key::definitions"
            ))
        )]
        peer: ed25519::PublicKey,
        interface: String,
    },
    /// A post-apply endpoint resolution (the by-identity rendezvous sweep,
    /// or a handshake-time resolve completing after the epoch applied)
    /// produced a dialable address for a peer, and the live interface was
    /// reconfigured with it in place.
    PeerEndpointResolved {
        #[borsh(
            serialize_with = "key::serialize",
            deserialize_with = "key::deserialize",
            schema(with_funcs(
                declaration = "key::declaration",
                definitions = "key::definitions"
            ))
        )]
        peer: ed25519::PublicKey,
        #[borsh(schema(with_funcs(
            declaration = "socket_addr::declaration",
            definitions = "socket_addr::definitions"
        )))]
        endpoint: SocketAddr,
    },
    /// A join-window invite peer merged onto the interface (see
    /// [`Event::InstallInvitePeer`]).
    InvitePeerInstalled {
        #[borsh(
            serialize_with = "key::serialize",
            deserialize_with = "key::deserialize",
            schema(with_funcs(
                declaration = "key::declaration",
                definitions = "key::definitions"
            ))
        )]
        peer: ed25519::PublicKey,
        interface: String,
    },
    /// A peer's signed CONTROL endpoint (its mesh address) was accepted with
    /// a value that differs from the last one observed for that identity —
    /// the node feeds it to the mesh transport's address book. Emitted
    /// only-on-change from every record/advert acceptance path and the boot
    /// restore; `peer` is the owner's raw ed25519 identity bytes.
    ControlEndpointObserved {
        peer: ValidatorIdentity,
        #[borsh(schema(with_funcs(
            declaration = "socket_addr::declaration",
            definitions = "socket_addr::definitions"
        )))]
        control_endpoint: SocketAddr,
    },
}

/// Everything that can happen TO the machine: the node's commands, delivered
/// gossip, the clock's ticks, and the outcomes of operations the machine
/// itself started.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub enum Event {
    /// Boot or epoch cutover: (re)build the mesh for this member set. A
    /// retarget SUPERSEDES any epoch still assembling — in-flight state and
    /// pending operations are dropped and assembly starts over. `persisted`
    /// carries the persisted-mesh bytes on the FIRST retarget of a host
    /// life (the cold-restart restore); later retargets are live cutovers
    /// with a working transport and carry `None`.
    Retarget {
        event: MeshEpochEvent,
        persisted: Option<Vec<u8>>,
    },
    /// A reachability-channel message arrived from a mesh peer.
    Deliver {
        #[borsh(
            serialize_with = "key::serialize",
            deserialize_with = "key::deserialize",
            schema(with_funcs(
                declaration = "key::declaration",
                definitions = "key::definitions"
            ))
        )]
        from: ed25519::PublicKey,
        bytes: Vec<u8>,
    },
    /// The consensus view advanced (drives expiry checks between cutovers).
    ViewTick(u64),
    /// Periodic controller kick: re-offer whatever this node is still
    /// waiting on — its un-acknowledged gossip (record, then advert) while
    /// the epoch is assembling, then each stalled peer's pending handshake
    /// message (request or response) after the mesh verifies. Mesh sends are
    /// best-effort datagrams — a message fired before the transport has a
    /// live connection to the peer (every boot `Retarget` fires before the
    /// p2p actors even start) is silently dropped, and when BOTH sides lose
    /// their initial record the first-contact heal in the record path never
    /// triggers on either. Safe at any cadence: every re-offer re-sends the
    /// STORED message verbatim, never re-signs — receivers dedup gossip by
    /// nonce (the mesh version is unchanged) and recognize handshake
    /// duplicates by hash (each side validates the triple exactly once, so
    /// the shared per-epoch `ReplayCache` never sees a nonce twice).
    Nudge,
    /// Install a JOIN-WINDOW tunnel peer, live and epoch-independent: the
    /// invite layer. The node has already authenticated the request (the
    /// invite blob's envelope on the joiner side; the token-verified intro
    /// datagram on the inviter side) — the machine only merges the peer
    /// onto the interface. Invite peers are the WEAKEST layer: an epoch's
    /// validated plan or a standby's signed record for the same identity
    /// supersedes them, and the entry dissolves once one exists. The apply
    /// outcome answers through [`Effect::ReplyInstall`] carrying `token`.
    InstallInvitePeer {
        token: CmdToken,
        /// The counterparty's ed25519 identity (its overlay ULA derives from
        /// this).
        #[borsh(
            serialize_with = "key::serialize",
            deserialize_with = "key::deserialize",
            schema(with_funcs(
                declaration = "key::declaration",
                definitions = "key::definitions"
            ))
        )]
        peer: ed25519::PublicKey,
        /// The counterparty's X25519 WireGuard key.
        wireguard_public_key: X25519PublicKey,
        /// Where to dial it: the blob's advertised endpoint on the joiner
        /// side; the intro datagram's observed source on the inviter side
        /// (WireGuard roams to the authenticated initiation either way).
        #[borsh(schema(with_funcs(
            declaration = "socket_addr::declaration",
            definitions = "socket_addr::definitions"
        )))]
        endpoint: SocketAddr,
    },
    /// Resolve a coordinated invite's inviter through the rendezvous plane,
    /// install the inviter as a join-window tunnel peer, then send the
    /// authenticated intro datagram over the same punched underlay socket.
    /// The final outcome answers through [`Effect::ReplyIntro`].
    BootstrapCoordinatedInvitePeer {
        token: CmdToken,
        #[borsh(
            serialize_with = "key::serialize",
            deserialize_with = "key::deserialize",
            schema(with_funcs(
                declaration = "key::declaration",
                definitions = "key::definitions"
            ))
        )]
        peer: ed25519::PublicKey,
        wireguard_public_key: X25519PublicKey,
        intro: Vec<u8>,
    },
    /// Send one datagram over the resolver socket. Used for invite intro ACKs
    /// after the receiving side has installed the join-window peer.
    SendResolverDatagram {
        #[borsh(schema(with_funcs(
            declaration = "socket_addr::declaration",
            definitions = "socket_addr::definitions"
        )))]
        endpoint: SocketAddr,
        bytes: Vec<u8>,
    },
    /// This node's OWN coordinator-observed reflexive mapping moved: the
    /// NAT rebound under a live epoch. The host sends this only when the
    /// address actually differs from the last one it observed, so the
    /// machine treats every one as a real rebind — it re-signs its record
    /// under a fresh nonce and re-advertises, which is what makes each peer
    /// run the post-lock re-advertisement path and rendezvous this node's
    /// new mapping by identity. `endpoint` is the observation itself; the
    /// record's advertised endpoint is the host's configuration and does
    /// not move with it (an endpoint-less node advertises no address at
    /// all — the coordinator IS its address book).
    ReflexiveChanged {
        #[borsh(schema(with_funcs(
            declaration = "socket_addr::declaration",
            definitions = "socket_addr::definitions"
        )))]
        endpoint: SocketAddr,
    },
    /// Outcome of an [`Effect::ResolveStart`].
    Resolved {
        req: ReqId,
        outcome: Result<Resolution, String>,
    },
    /// Outcome of an [`Effect::RendezvousStart`].
    RendezvousResolved {
        req: ReqId,
        #[borsh(schema(with_funcs(
            declaration = "result_socket_addr::declaration",
            definitions = "result_socket_addr::definitions"
        )))]
        outcome: Result<SocketAddr, String>,
    },
    /// Outcome of an [`Effect::UdpSendAwait`]: the first non-rendezvous
    /// datagram back from that endpoint, or the timeout/failure.
    DatagramReplied {
        req: ReqId,
        outcome: Result<Vec<u8>, String>,
    },
    /// Outcome of an [`Effect::WgApply`]; the error is the host backend's
    /// refusal, Debug-formatted. The host feeds this back IMMEDIATELY —
    /// before draining any other event — so a push round-trips inside the
    /// step cascade that requested it.
    WgApplied {
        req: ReqId,
        outcome: Result<(), String>,
    },
    /// Drain and exit; the interface is torn down on the way out.
    Shutdown,
}

/// Everything the machine asks the host to DO. Performed in order; the
/// variants that produce outcomes name the event that carries them back.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, BorshSchema)]
pub enum Effect {
    /// Send `bytes` to `to` on the reachability mesh channel.
    MeshSend {
        #[borsh(
            serialize_with = "key::serialize",
            deserialize_with = "key::deserialize",
            schema(with_funcs(
                declaration = "key::declaration",
                definitions = "key::definitions"
            ))
        )]
        to: ed25519::PublicKey,
        bytes: Vec<u8>,
    },
    /// Surface an observability event to the node.
    Observe(ReachabilityEvent),
    /// Push the interface's full desired peer set: bring the interface up
    /// (`bring_up`) or reconfigure the live one in place. The host owns the
    /// interface name, listen port, local overlay addresses, and the
    /// private key; the machine owns WHICH peers exist. The outcome returns
    /// as [`Event::WgApplied`] within the same step cascade.
    WgApply {
        req: ReqId,
        bring_up: bool,
        peers: Vec<PeerTunnelConfig>,
    },
    /// Tear the interface down, best-effort: every requester is leaving the
    /// mesh (stand-down) or the process (shutdown), where the teardown's
    /// error detail does not change what happens next.
    WgRemove,
    /// Resolve `peer`'s dialable UDP address given its advertised WireGuard
    /// endpoint; outcome returns as [`Event::Resolved`].
    ResolveStart {
        req: ReqId,
        peer: NodeKey,
        #[borsh(schema(with_funcs(
            declaration = "socket_addr::declaration",
            definitions = "socket_addr::definitions"
        )))]
        advertised: SocketAddr,
    },
    /// Resolve `peer` strictly through rendezvous (no trusted advertised
    /// endpoint); outcome returns as [`Event::RendezvousResolved`].
    RendezvousStart { req: ReqId, peer: NodeKey },
    /// Send one datagram from the resolver's underlay socket, fire and
    /// forget.
    UdpSend {
        #[borsh(schema(with_funcs(
            declaration = "socket_addr::declaration",
            definitions = "socket_addr::definitions"
        )))]
        endpoint: SocketAddr,
        bytes: Vec<u8>,
    },
    /// Send one datagram and wait for the first non-rendezvous datagram
    /// back from that same endpoint; outcome returns as
    /// [`Event::DatagramReplied`].
    UdpSendAwait {
        req: ReqId,
        #[borsh(schema(with_funcs(
            declaration = "socket_addr::declaration",
            definitions = "socket_addr::definitions"
        )))]
        endpoint: SocketAddr,
        bytes: Vec<u8>,
        timeout_ms: u64,
    },
    /// Answer the [`Event::InstallInvitePeer`] command carrying `token`.
    ReplyInstall {
        token: CmdToken,
        outcome: Result<(), String>,
    },
    /// Answer the [`Event::BootstrapCoordinatedInvitePeer`] command
    /// carrying `token`.
    ReplyIntro {
        token: CmdToken,
        outcome: Result<Vec<u8>, String>,
    },
    /// Persist the applied mesh snapshot (the bytes
    /// [`crate::store::decode_verified`] reads back). Emitted only when
    /// [`MachineConfig::persist`] is set; a host write failure surfaces as
    /// [`ReachabilityEvent::PersistFailed`] from the host itself.
    Persist { bytes: Vec<u8> },
}
