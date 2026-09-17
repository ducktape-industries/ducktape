//! The host executor: drives the netstack machine over the node's
//! command/event channels and PERFORMS what it decides — mesh sends,
//! interface pushes (through a [`WireGuardEffect`]), resolver operations
//! (through an [`EndpointResolver`]), command replies, and persisted-mesh
//! writes. Everything the machine is not allowed to touch lives here: the
//! clock stamp, the filesystem, the sockets, the WireGuard private key.
//!
//! Runtime contract: the node runs [`run`] as the ROOT future of a dedicated
//! plain-tokio runtime on its own OS thread (the same split as the node's
//! app-surface thread), talking to the commonware runner over the two mpsc
//! channels. The future is not required to be `Send` — nothing here may
//! assume `tokio::spawn` onto a shared runtime, so the resolver runs as an
//! in-future PUMP joined with the command loop: resolver operations queue to
//! it and their completions feed back as machine events. That split is what
//! keeps a slow resolve from stalling the plane — commands keep draining
//! while an operation is in flight — while resolver operations themselves
//! stay serialized in start order, and it needs neither `Send` nor
//! `'static` from the resolver.
//!
//! One machine obligation is honored HERE: an interface push
//! ([`Effect::WgApply`]) is performed synchronously and its outcome stepped
//! back into the machine before anything else drains, so a push round-trips
//! inside the step cascade that requested it.
//!
//! The machine is driven through [`NetstackMachine`], so the loop never
//! learns whether it holds the native [`netstack_machine::Machine`] or the wasm guest
//! ([`NetstackBackend`]). Two things it handles beyond stepping: a backend
//! can FAULT (a trap, an exhausted budget), after which its state is
//! unknown — the loop stops and retires its resources; and the node
//! can SWAP the backend mid-life ([`ReachabilityCommand::SwapBackend`]) —
//! the machine's snapshot restores into the new backend and the epoch
//! continues exactly where it was, no retarget and no interface push.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use commonware_cryptography::{Signer as _, ed25519};
use nat_traversal::NodeKey;
use netstack_machine::{
    CmdToken, Effect, Event, MachineConfig, MeshEpochEvent, NetstackMachine, ReachabilityEvent,
    ReqId, StepError, binding,
};
#[cfg(feature = "native-tests")]
use netstack_machine::{Machine, SnapshotError};
use netstack_wasm::{GuestError, NetstackGuest};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use wireguard::effect::{
    PeerTunnelConfig, WireGuardEffect, apply_peer_tunnels, update_peer_tunnels,
};
use wireguard::{AllowedIp, Endpoint, IdentitySigner, OverlayPolicy, PortPolicy, UpgradeError};

use crate::keys::{KeyError, WireGuardKeypair};
use crate::rendezvous::{EndpointResolver, Resolution};
use crate::store::{self, StoreError};

/// Everything the node resolves ONCE at boot and hands the plane.
pub struct ReachabilityConfig {
    /// The chain id — doubles as the advertisement namespace and the ULA
    /// derivation input, exactly as it does for the commonware mesh.
    pub chain_id: String,
    /// The node's ed25519 identity: signs records, advertisements, and
    /// handshake messages. Its public key IS the member identity.
    pub signer: ed25519::PrivateKey,
    /// Where the X25519 keypair lives (beside `identity.key`);
    /// `keys::WireGuardKeypair::load_or_generate` runs against this path.
    pub wireguard_key_file: PathBuf,
    /// The local WireGuard UDP bind port — always needed to bring the
    /// interface up, independent of whether an endpoint is advertised.
    pub wireguard_port: u16,
    /// The node's own advertised WireGuard UDP endpoint — `None` for an
    /// endpoint-less (NAT'd) node: it advertises no address, installs every
    /// peer FROM the records, and initiates; peers install it without an
    /// endpoint and WireGuard roams to its authenticated initiation.
    pub wireguard_advertised: Option<Endpoint>,
    /// The node's own advertised control-mesh endpoint.
    pub control_endpoint: Endpoint,
    /// Rendezvous coordinators (from `Resolved.coordinated`), possibly none:
    /// with an empty list every peer resolves to its advertised endpoint.
    pub coordinators: Vec<SocketAddr>,
    /// The endpoint policy advertisements and handshakes validate against.
    pub port_policy: PortPolicy,
    /// Where the last applied epoch's verified mesh is persisted (and read
    /// back for the cold-restart re-apply). `None` disables persistence —
    /// the plane then only ever assembles from live gossip.
    pub persist_file: Option<PathBuf>,
    /// A transport identity whose DELIVERIES are admitted even though it is
    /// no plane participant: the mesh's derived lobby key, which a parked
    /// standby connects under while its own key is still untracked. Purely
    /// an ingress allowance — every message still authenticates by its
    /// owner's content signature, and standby-directed replies route back
    /// over whichever transport identity delivered the standby's record.
    pub gossip_ingress: Option<ed25519::PublicKey>,
    /// Which machine drives the plane.
    pub backend: NetstackBackend,
}

/// Which implementation of the netstack machine the plane runs. The guest
/// is the arc's upgradeable form: the same contract behind the wasm
/// boundary, swappable without a binary release. A guest that cannot boot
/// or faults stops the plane. Native is reserved for scenario tests.
#[derive(Clone)]
pub enum NetstackBackend {
    /// The deterministic scenario harness implementation; never selected by a node.
    #[cfg(feature = "native-tests")]
    Native,
    /// The `ducktape:netstack` component these bytes carry, stepped under
    /// `step_fuel` units of wasm fuel per event — exhaustion is a fault.
    Guest { component: Vec<u8>, step_fuel: u64 },
}

/// Host execution status. This is not part of the guest protocol ABI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendStatus {
    Starting,
    Running {
        code_hash: [u8; 32],
    },
    /// Only the deterministic scenario harness selects the native machine.
    #[cfg(feature = "native-tests")]
    Native,
    Stopped,
    Failed(String),
}

impl BackendStatus {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running { .. } => "guest",
            #[cfg(feature = "native-tests")]
            Self::Native => "native",
            Self::Stopped => "stopped",
            Self::Failed(_) => "failed",
        }
    }

    pub fn code_hash(&self) -> Option<[u8; 32]> {
        match self {
            Self::Running { code_hash } => Some(*code_hash),
            Self::Starting | Self::Stopped | Self::Failed(_) => None,
            #[cfg(feature = "native-tests")]
            Self::Native => None,
        }
    }
}

impl NetstackBackend {
    fn running_status(&self) -> BackendStatus {
        match self {
            #[cfg(feature = "native-tests")]
            Self::Native => BackendStatus::Native,
            Self::Guest { component, .. } => BackendStatus::Running {
                code_hash: Sha256::digest(component).into(),
            },
        }
    }

    /// The backend's name as the logs carry it.
    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "native-tests")]
            Self::Native => "native",
            Self::Guest { .. } => "guest",
        }
    }
}

impl std::fmt::Debug for NetstackBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "native-tests")]
            Self::Native => f.write_str("Native"),
            Self::Guest {
                component,
                step_fuel,
            } => write!(f, "Guest({} bytes, {step_fuel} fuel/step)", component.len()),
        }
    }
}

/// The apply outcome an [`ReachabilityCommand::InstallInvitePeer`] caller
/// awaits — wrapped so the command enum keeps its `Debug`.
pub struct InstallReply(pub tokio::sync::oneshot::Sender<Result<(), String>>);
#[derive(Debug)]
pub struct CoordinatedInviteReply(pub tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>);

impl std::fmt::Debug for InstallReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstallReply")
    }
}

/// The outcome a [`ReachabilityCommand::SwapBackend`] caller awaits: `Ok`
/// once the new backend runs the plane, `Err` naming why the swap was
/// refused (the current machine continues) or why the old guest faulted
/// while taking its snapshot (the plane stops).
pub struct SwapReply(pub tokio::sync::oneshot::Sender<Result<(), String>>);

impl std::fmt::Debug for SwapReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SwapReply")
    }
}

/// Node -> plane.
#[derive(Debug)]
pub enum ReachabilityCommand {
    /// Boot or epoch cutover: (re)build the mesh for this member set. A
    /// retarget SUPERSEDES any epoch still assembling — tear down in-flight
    /// state and start over.
    Retarget(MeshEpochEvent),
    /// Install a JOIN-WINDOW tunnel peer, live and epoch-independent: the
    /// invite layer. The node has already authenticated the request (the
    /// invite blob's envelope on the joiner side; the token-verified intro
    /// datagram on the inviter side) — the plane only merges the peer
    /// onto the interface. Invite peers are the WEAKEST layer: an epoch's
    /// validated plan or a standby's signed record for the same identity
    /// supersedes them, and the entry dissolves once one exists.
    InstallInvitePeer {
        /// The counterparty's ed25519 identity (its overlay ULA derives from
        /// this).
        peer: ed25519::PublicKey,
        /// The counterparty's X25519 WireGuard key.
        wireguard_public_key: wireguard::X25519PublicKey,
        /// Where to dial it: the blob's advertised endpoint on the joiner
        /// side; the intro datagram's observed source on the inviter side
        /// (WireGuard roams to the authenticated initiation either way).
        endpoint: SocketAddr,
        /// Resolved with the apply outcome (the inviter acks the intro only
        /// after the peer is really on the interface).
        reply: InstallReply,
    },
    /// Resolve a coordinated invite's inviter through the rendezvous plane,
    /// install the inviter as a join-window tunnel peer, then send the
    /// authenticated intro datagram over the same punched underlay socket.
    BootstrapCoordinatedInvitePeer {
        peer: ed25519::PublicKey,
        wireguard_public_key: wireguard::X25519PublicKey,
        intro: Vec<u8>,
        reply: CoordinatedInviteReply,
    },
    /// Send one datagram over the resolver socket. Used for invite intro ACKs
    /// after the receiving side has installed the join-window peer.
    SendResolverDatagram {
        endpoint: SocketAddr,
        bytes: Vec<u8>,
    },
    /// A reachability-channel message arrived from a mesh peer.
    Deliver {
        from: ed25519::PublicKey,
        bytes: Vec<u8>,
    },
    /// The consensus view advanced (drives expiry checks between cutovers).
    ViewTick(u64),
    /// This node's own coordinator-observed reflexive mapping moved (see
    /// [`Event::ReflexiveChanged`]). Sent ONLY on a real change — the
    /// resolver's status watch is the observation, and the plane treats
    /// every one of these as a NAT rebind worth a fresh record.
    ReflexiveChanged { endpoint: SocketAddr },
    /// Periodic controller kick: re-offer whatever this node is still
    /// waiting on (see [`Event::Nudge`] for the full contract).
    Nudge,
    /// Swap the machine behind the plane for `backend`, mid-life: the
    /// current machine's snapshot restores into the new one and the epoch
    /// continues where it was — no retarget, no interface push. A backend
    /// that cannot restore the snapshot is refused and the current machine
    /// continues; a machine that cannot even take one has faulted, and the
    /// plane stops because its current state is unknown. The reply says why.
    SwapBackend {
        backend: NetstackBackend,
        reply: SwapReply,
    },
    /// Drain and exit; the interface is torn down on the way out.
    Shutdown,
}

/// Why the plane stopped. Everything recoverable is an observed event, not
/// an error: a refused push, an unreadable state file, a failed resolve all
/// surface as [`ReachabilityEvent`]s and the plane keeps running.
#[derive(Debug, thiserror::Error)]
pub enum ReachabilityError {
    #[error("wireguard keystore: {0}")]
    Key(#[from] KeyError),
    #[error("protocol: {0:?}")]
    Upgrade(UpgradeError),
    #[error("the node dropped a reachability channel")]
    ChannelClosed,
    /// The deployed guest cannot continue; the plane stops.
    #[error("netstack backend fault: {0}")]
    Backend(String),
}

impl From<UpgradeError> for ReachabilityError {
    fn from(err: UpgradeError) -> Self {
        Self::Upgrade(err)
    }
}

/// unix time in milliseconds — the machine's step clock: record-nonce seeds
/// and the rendezvous budget both count in it.
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One queued resolver operation, performed by the pump in start order.
enum ResolverOp {
    Resolve {
        req: ReqId,
        peer: NodeKey,
        advertised: SocketAddr,
    },
    Rendezvous {
        req: ReqId,
        peer: NodeKey,
    },
    Send {
        endpoint: SocketAddr,
        bytes: Vec<u8>,
    },
    SendAwait {
        req: ReqId,
        endpoint: SocketAddr,
        bytes: Vec<u8>,
        timeout: Duration,
    },
}

/// A finished resolver operation, fed back into the machine as its event.
enum Completion {
    Resolved {
        req: ReqId,
        outcome: Result<Resolution, String>,
    },
    Rendezvous {
        req: ReqId,
        outcome: Result<SocketAddr, String>,
    },
    Datagram {
        req: ReqId,
        outcome: Result<Vec<u8>, String>,
    },
}

/// The reply half of a command still awaiting its machine-decided outcome,
/// keyed by the [`CmdToken`] minted when the command was translated.
enum PendingReply {
    Install(InstallReply),
    Intro(CoordinatedInviteReply),
}

/// The host-owned constants of every interface push: the machine decides
/// WHICH peers exist; the interface name, listen port, local overlay
/// addresses, and the private key never leave the host.
struct WgPlan {
    interface: String,
    private_key: [u8; 32],
    listen_port: u16,
    local_ips: Vec<AllowedIp>,
}

/// Drive the reachability plane until `Shutdown` (clean exit) or a channel
/// closes (error). One call outlives every epoch; `Retarget` events move it
/// between epochs. The per-epoch protocol is the netstack machine's
/// ([`netstack_machine::Machine`]); this future translates commands to machine events, stamps
/// the clock, performs the machine's effects in order, and pumps the
/// resolver.
pub async fn run<E, R>(
    config: ReachabilityConfig,
    effect: E,
    resolver: R,
    commands: mpsc::Receiver<ReachabilityCommand>,
    events: mpsc::Sender<ReachabilityEvent>,
) -> Result<(), ReachabilityError>
where
    E: WireGuardEffect,
    R: EndpointResolver,
{
    run_observed(config, effect, resolver, commands, events, |_| {}).await
}

/// Drive the plane and publish actual execution transitions, including faults.
/// Running is published only after successful guest boot/replacement.
pub async fn run_observed<E, R>(
    config: ReachabilityConfig,
    effect: E,
    resolver: R,
    commands: mpsc::Receiver<ReachabilityCommand>,
    events: mpsc::Sender<ReachabilityEvent>,
    mut observe: impl FnMut(BackendStatus),
) -> Result<(), ReachabilityError>
where
    E: WireGuardEffect,
    R: EndpointResolver,
{
    observe(BackendStatus::Starting);
    let outcome = run_inner(config, effect, resolver, commands, events, &mut observe).await;
    match &outcome {
        Ok(()) => observe(BackendStatus::Stopped),
        Err(error) => {
            tracing::error!(
                target: "ducktape::reachability",
                event = "netstack_backend_fault",
                error = %error,
                "netstack execution stopped"
            );
            observe(BackendStatus::Failed(error.to_string()));
        }
    }
    outcome
}

async fn run_inner<E, R>(
    config: ReachabilityConfig,
    effect: E,
    resolver: R,
    commands: mpsc::Receiver<ReachabilityCommand>,
    events: mpsc::Sender<ReachabilityEvent>,
    observe: &mut impl FnMut(BackendStatus),
) -> Result<(), ReachabilityError>
where
    E: WireGuardEffect,
    R: EndpointResolver,
{
    let (keypair, _generated) = WireGuardKeypair::load_or_generate(&config.wireguard_key_file)?;
    let me = binding::identity_of(&config.signer.public_key());
    let overlay = OverlayPolicy::ula_v6(config.chain_id.clone());
    let plan = WgPlan {
        interface: binding::interface_name(&config.chain_id),
        private_key: keypair.private_key_bytes(),
        listen_port: config.wireguard_port,
        // the plane's overlay is ula_v6: the local side is the same
        // identity-derived /128 every validated plan carries.
        local_ips: overlay.identity_allowed_ips(me),
    };
    let factory = MachineFactory {
        signer: config.signer,
        config: MachineConfig {
            chain_id: config.chain_id,
            wireguard_public: keypair.public_key(),
            wireguard_advertised: config.wireguard_advertised,
            control_endpoint: config.control_endpoint,
            coordinators: config.coordinators,
            port_policy: config.port_policy,
            persist: config.persist_file.is_some(),
            gossip_ingress: config.gossip_ingress,
        },
        backend: config.backend,
    };
    let (op_tx, op_rx) = mpsc::unbounded_channel::<ResolverOp>();
    let (done_tx, done_rx) = mpsc::unbounded_channel::<Completion>();
    let mut host = Host {
        effect,
        events,
        op_tx,
        replies: BTreeMap::new(),
        next_token: 0,
        restore_file: config.persist_file.clone(),
        persist_file: config.persist_file,
        plan,
    };
    // Ending the command loop cancels pending resolver I/O immediately. No
    // completion from this instance can escape into a subsequent plane.
    let outcome = tokio::select! {
        outcome = host_loop(factory, &mut host, commands, done_rx, observe) => outcome,
        () = resolver_pump(resolver, op_rx, done_tx) => Err(ReachabilityError::ChannelClosed),
    };
    host.replies.clear();
    let _ = host.effect.remove_interface();
    outcome
}

/// The command loop: completions drain before commands (an outcome the
/// machine is waiting on should never queue behind fresh work), every event
/// is stamped, and every effect list is performed before the next drain. A
/// backend fault stops the loop; a swap command replaces the guest between
/// two events, which is the only place
/// its state is at rest.
async fn host_loop<E: WireGuardEffect>(
    factory: MachineFactory,
    host: &mut Host<E>,
    mut commands: mpsc::Receiver<ReachabilityCommand>,
    mut done_rx: mpsc::UnboundedReceiver<Completion>,
    observe: &mut impl FnMut(BackendStatus),
) -> Result<(), ReachabilityError> {
    let mut machine = factory.boot()?;
    tracing::info!(
        target: "ducktape::reachability",
        event = "netstack_backend",
        backend = factory.backend.name(),
        "netstack machine started"
    );
    observe(factory.backend.running_status());
    loop {
        let input = tokio::select! {
            biased;
            Some(done) = done_rx.recv() => Input::Step { event: Box::new(completion_event(done)), exit: false },
            cmd = commands.recv() => match cmd {
                Some(cmd) => host.input(cmd).await?,
                None => return Err(ReachabilityError::ChannelClosed),
            },
        };
        match input {
            Input::Step { event, exit } => {
                machine = host.step(machine, *event).await?;
                if exit {
                    return Ok(());
                }
            }
            Input::Swap { backend, reply } => {
                machine = host
                    .swap(&factory, machine, backend, reply, observe)
                    .await?;
            }
        }
    }
}

/// What one turn of the loop does: step the machine, or swap it.
enum Input {
    Step {
        event: Box<Event>,
        exit: bool,
    },
    Swap {
        backend: NetstackBackend,
        reply: SwapReply,
    },
}

/// Instantiates a configured guest or restores a candidate from opaque state.
struct MachineFactory {
    signer: ed25519::PrivateKey,
    config: MachineConfig,
    backend: NetstackBackend,
}

impl MachineFactory {
    fn signer(&self) -> Box<dyn IdentitySigner> {
        Box::new(self.signer.clone())
    }

    #[cfg(feature = "native-tests")]
    fn native(&self) -> Box<dyn NetstackMachine> {
        Box::new(Machine::new(self.signer(), self.config.clone()))
    }

    /// A deployed component must instantiate successfully before it runs.
    fn boot(&self) -> Result<Box<dyn NetstackMachine>, ReachabilityError> {
        match &self.backend {
            #[cfg(feature = "native-tests")]
            NetstackBackend::Native => Ok(self.native()),
            NetstackBackend::Guest {
                component,
                step_fuel,
            } => {
                let guest = NetstackGuest::with_fuel(
                    component,
                    self.signer(),
                    self.config.clone(),
                    *step_fuel,
                )
                .map_err(|error| {
                    tracing::error!(
                        target: "ducktape::reachability",
                        event = "netstack_guest_boot_failed",
                        error = %error,
                        "netstack guest could not start"
                    );
                    ReachabilityError::Backend(error.to_string())
                })?;
                Ok(Box::new(guest))
            }
        }
    }

    /// `backend` continuing from `snapshot`.
    fn restore(
        &self,
        backend: &NetstackBackend,
        snapshot: &[u8],
    ) -> Result<Box<dyn NetstackMachine>, RestoreError> {
        match backend {
            #[cfg(feature = "native-tests")]
            NetstackBackend::Native => {
                let machine = Machine::restore(self.signer(), self.config.clone(), snapshot)?;
                Ok(Box::new(machine))
            }
            NetstackBackend::Guest {
                component,
                step_fuel,
            } => {
                let guest = NetstackGuest::restore(
                    component,
                    self.signer(),
                    self.config.clone(),
                    snapshot,
                    *step_fuel,
                )?;
                Ok(Box::new(guest))
            }
        }
    }
}

/// Why a backend could not continue from a snapshot.
#[derive(Debug, thiserror::Error)]
enum RestoreError {
    #[cfg(feature = "native-tests")]
    #[error("native machine: {0}")]
    Native(#[from] SnapshotError),
    #[error(transparent)]
    Guest(#[from] GuestError),
}

/// What driving one event through the machine came to.
enum Drive {
    /// Every effect performed.
    Done,
    /// The backend faulted mid-drive: its state is unknown, and whatever
    /// effects the step had not yet produced are lost with it.
    Faulted(String),
}

/// One step's outcomes, sorted for the loop: effects to perform, a backend
/// fault, or a protocol breach. Both stop the plane.
enum Stepped {
    Effects(Vec<Effect>),
    Faulted(String),
}

fn stepped(result: Result<Vec<Effect>, StepError>) -> Result<Stepped, ReachabilityError> {
    match result {
        Ok(effects) => Ok(Stepped::Effects(effects)),
        Err(StepError::Fault(reason)) => Ok(Stepped::Faulted(reason)),
        Err(StepError::Protocol(err)) => Err(err.into()),
    }
}

fn completion_event(done: Completion) -> Event {
    match done {
        Completion::Resolved { req, outcome } => Event::Resolved { req, outcome },
        Completion::Rendezvous { req, outcome } => Event::RendezvousResolved { req, outcome },
        Completion::Datagram { req, outcome } => Event::DatagramReplied { req, outcome },
    }
}

/// The resolver's half: operations arrive in start order and run one at a
/// time — exactly the serialization one `&mut` resolver implies — while the
/// command loop keeps draining beside it.
async fn resolver_pump<R: EndpointResolver>(
    mut resolver: R,
    mut ops: mpsc::UnboundedReceiver<ResolverOp>,
    done: mpsc::UnboundedSender<Completion>,
) {
    while let Some(op) = ops.recv().await {
        match op {
            ResolverOp::Resolve {
                req,
                peer,
                advertised,
            } => {
                let outcome = resolver.resolve(peer, advertised).await;
                let _ = done.send(Completion::Resolved { req, outcome });
            }
            ResolverOp::Rendezvous { req, peer } => {
                let outcome = resolver.resolve_rendezvous_endpoint(peer).await;
                let _ = done.send(Completion::Rendezvous { req, outcome });
            }
            ResolverOp::Send { endpoint, bytes } => {
                let _ = resolver.send_datagram(endpoint, bytes).await;
            }
            ResolverOp::SendAwait {
                req,
                endpoint,
                bytes,
                timeout,
            } => {
                let outcome = resolver
                    .send_datagram_and_recv(endpoint, bytes, timeout)
                    .await;
                let _ = done.send(Completion::Datagram { req, outcome });
            }
        }
    }
}

/// Everything the loop performs effects WITH.
struct Host<E> {
    effect: E,
    events: mpsc::Sender<ReachabilityEvent>,
    op_tx: mpsc::UnboundedSender<ResolverOp>,
    replies: BTreeMap<CmdToken, PendingReply>,
    next_token: u64,
    /// The state file still to be offered as the boot restore — taken by
    /// the first retarget, so the restore is structurally once per life.
    restore_file: Option<PathBuf>,
    /// Where applied-mesh snapshots are written, for the whole life.
    persist_file: Option<PathBuf>,
    plan: WgPlan,
}

impl<E: WireGuardEffect> Host<E> {
    /// Sort one node command into the loop's input: the machine event it
    /// translates to (minting reply tokens for the commands that answer
    /// later, reading the persisted mesh once for the boot retarget), or
    /// the one command that swaps the machine instead of stepping it.
    async fn input(&mut self, cmd: ReachabilityCommand) -> Result<Input, ReachabilityError> {
        let step = |event: Event, exit: bool| {
            Ok(Input::Step {
                event: Box::new(event),
                exit,
            })
        };
        match cmd {
            ReachabilityCommand::Retarget(event) => {
                let persisted = self.read_restore_file().await?;
                step(Event::Retarget { event, persisted }, false)
            }
            ReachabilityCommand::InstallInvitePeer {
                peer,
                wireguard_public_key,
                endpoint,
                reply,
            } => {
                let token = self.mint_token(PendingReply::Install(reply));
                step(
                    Event::InstallInvitePeer {
                        token,
                        peer,
                        wireguard_public_key,
                        endpoint,
                    },
                    false,
                )
            }
            ReachabilityCommand::BootstrapCoordinatedInvitePeer {
                peer,
                wireguard_public_key,
                intro,
                reply,
            } => {
                let token = self.mint_token(PendingReply::Intro(reply));
                step(
                    Event::BootstrapCoordinatedInvitePeer {
                        token,
                        peer,
                        wireguard_public_key,
                        intro,
                    },
                    false,
                )
            }
            ReachabilityCommand::SendResolverDatagram { endpoint, bytes } => {
                step(Event::SendResolverDatagram { endpoint, bytes }, false)
            }
            ReachabilityCommand::Deliver { from, bytes } => {
                step(Event::Deliver { from, bytes }, false)
            }
            ReachabilityCommand::ViewTick(view) => step(Event::ViewTick(view), false),
            ReachabilityCommand::ReflexiveChanged { endpoint } => {
                step(Event::ReflexiveChanged { endpoint }, false)
            }
            ReachabilityCommand::Nudge => step(Event::Nudge, false),
            ReachabilityCommand::SwapBackend { backend, reply } => {
                Ok(Input::Swap { backend, reply })
            }
            ReachabilityCommand::Shutdown => step(Event::Shutdown, true),
        }
    }

    fn mint_token(&mut self, reply: PendingReply) -> CmdToken {
        self.next_token += 1;
        let token = CmdToken(self.next_token);
        self.replies.insert(token, reply);
        token
    }

    /// The persisted-mesh bytes ride the FIRST retarget of this host life
    /// only — later retargets are live cutovers with a working transport,
    /// and restoring over them would tear down good tunnels. The machine
    /// decodes and verifies; an unreadable FILE is the host's own refusal.
    async fn read_restore_file(&mut self) -> Result<Option<Vec<u8>>, ReachabilityError> {
        let Some(path) = self.restore_file.take() else {
            return Ok(None);
        };
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => {
                let reason = StoreError::Io {
                    path: path.display().to_string(),
                    source,
                }
                .to_string();
                self.observe(ReachabilityEvent::RestoreFailed { reason })
                    .await?;
                Ok(None)
            }
        }
    }

    /// Drive an event. A guest fault stops execution without replaying effects.
    async fn step(
        &mut self,
        mut machine: Box<dyn NetstackMachine>,
        event: Event,
    ) -> Result<Box<dyn NetstackMachine>, ReachabilityError> {
        match self.drive(&mut *machine, event).await? {
            Drive::Done => Ok(machine),
            Drive::Faulted(reason) => Err(ReachabilityError::Backend(reason)),
        }
    }

    /// Swap the machine for one `backend` restores from its snapshot; the
    /// epoch continues where it was. A backend that refuses the snapshot
    /// leaves the current machine in place. A snapshot fault stops the plane:
    /// the current guest state is unknown and cannot safely continue.
    async fn swap(
        &mut self,
        factory: &MachineFactory,
        mut machine: Box<dyn NetstackMachine>,
        backend: NetstackBackend,
        reply: SwapReply,
        observe: &mut impl FnMut(BackendStatus),
    ) -> Result<Box<dyn NetstackMachine>, ReachabilityError> {
        let snapshot = match machine.snapshot() {
            Ok(snapshot) => snapshot,
            Err(StepError::Fault(reason)) => {
                let _ = reply.0.send(Err(format!("snapshot: {reason}")));
                return Err(ReachabilityError::Backend(reason));
            }
            Err(StepError::Protocol(err)) => return Err(err.into()),
        };
        match factory.restore(&backend, &snapshot) {
            Ok(swapped) => {
                tracing::info!(
                    target: "ducktape::reachability",
                    event = "netstack_backend_swapped",
                    backend = backend.name(),
                    snapshot_bytes = snapshot.len(),
                    "netstack machine swapped mid-life; the epoch continues"
                );
                observe(backend.running_status());
                let _ = reply.0.send(Ok(()));
                Ok(swapped)
            }
            Err(err) => {
                tracing::warn!(
                    target: "ducktape::reachability",
                    event = "netstack_backend_swap_refused",
                    backend = backend.name(),
                    error = %err,
                    "netstack backend refused the snapshot; the current machine continues"
                );
                let _ = reply.0.send(Err(err.to_string()));
                Ok(machine)
            }
        }
    }

    /// Step one event and perform what it decides.
    async fn drive(
        &mut self,
        machine: &mut dyn NetstackMachine,
        event: Event,
    ) -> Result<Drive, ReachabilityError> {
        let effects = match stepped(machine.step(event, unix_now_ms()))? {
            Stepped::Effects(effects) => effects,
            Stepped::Faulted(reason) => return Ok(Drive::Faulted(reason)),
        };
        self.perform(machine, effects).await
    }

    /// Perform one step's effects IN ORDER. An interface push is performed
    /// here and its outcome stepped straight back in; the new effects run
    /// to completion before the remainder of the outer list — exactly the
    /// inline order the machine's step cascade expresses.
    async fn perform(
        &mut self,
        machine: &mut dyn NetstackMachine,
        effects: Vec<Effect>,
    ) -> Result<Drive, ReachabilityError> {
        let mut stack: Vec<std::vec::IntoIter<Effect>> = vec![effects.into_iter()];
        while let Some(top) = stack.last_mut() {
            let Some(effect) = top.next() else {
                stack.pop();
                continue;
            };
            match effect {
                Effect::MeshSend { to, bytes } => {
                    self.observe(ReachabilityEvent::Send { to, bytes }).await?
                }
                Effect::Observe(event) => self.observe(event).await?,
                Effect::WgApply {
                    req,
                    bring_up,
                    peers,
                } => {
                    let outcome = self.push_interface(bring_up, &peers);
                    let applied = Event::WgApplied { req, outcome };
                    let more = match stepped(machine.step(applied, unix_now_ms()))? {
                        Stepped::Effects(more) => more,
                        Stepped::Faulted(reason) => return Ok(Drive::Faulted(reason)),
                    };
                    stack.push(more.into_iter());
                }
                Effect::WgRemove => {
                    // best-effort by contract: the requester is leaving the
                    // mesh or the process either way.
                    let _ = self.effect.remove_interface();
                }
                Effect::ResolveStart {
                    req,
                    peer,
                    advertised,
                } => self.start_op(ResolverOp::Resolve {
                    req,
                    peer,
                    advertised,
                }),
                Effect::RendezvousStart { req, peer } => {
                    self.start_op(ResolverOp::Rendezvous { req, peer })
                }
                Effect::UdpSend { endpoint, bytes } => {
                    self.start_op(ResolverOp::Send { endpoint, bytes })
                }
                Effect::UdpSendAwait {
                    req,
                    endpoint,
                    bytes,
                    timeout_ms,
                } => self.start_op(ResolverOp::SendAwait {
                    req,
                    endpoint,
                    bytes,
                    timeout: Duration::from_millis(timeout_ms),
                }),
                Effect::ReplyInstall { token, outcome } => self.reply_install(token, outcome),
                Effect::ReplyIntro { token, outcome } => self.reply_intro(token, outcome),
                Effect::Persist { bytes } => self.persist(bytes).await?,
            }
        }
        Ok(Drive::Done)
    }

    async fn observe(&mut self, event: ReachabilityEvent) -> Result<(), ReachabilityError> {
        self.events
            .send(event)
            .await
            .map_err(|_| ReachabilityError::ChannelClosed)
    }

    fn start_op(&mut self, op: ResolverOp) {
        // the pump outlives the loop by construction; a send can only fail
        // during teardown, where the outcome no longer matters.
        let _ = self.op_tx.send(op);
    }

    fn push_interface(&mut self, bring_up: bool, peers: &[PeerTunnelConfig]) -> Result<(), String> {
        let plan = &self.plan;
        let pushed = match bring_up {
            true => apply_peer_tunnels(
                &mut self.effect,
                plan.interface.clone(),
                plan.private_key,
                plan.listen_port,
                &plan.local_ips,
                peers,
            ),
            false => update_peer_tunnels(
                &mut self.effect,
                plan.interface.clone(),
                plan.private_key,
                plan.listen_port,
                &plan.local_ips,
                peers,
            ),
        };
        pushed.map_err(|err| format!("{err:?}"))
    }

    fn reply_install(&mut self, token: CmdToken, outcome: Result<(), String>) {
        match self.replies.remove(&token) {
            Some(PendingReply::Install(reply)) => {
                let _ = reply.0.send(outcome);
            }
            Some(PendingReply::Intro(_)) => {
                debug_assert!(false, "an install reply answered an intro command");
            }
            None => {}
        }
    }

    fn reply_intro(&mut self, token: CmdToken, outcome: Result<Vec<u8>, String>) {
        match self.replies.remove(&token) {
            Some(PendingReply::Intro(reply)) => {
                let _ = reply.0.send(outcome);
            }
            Some(PendingReply::Install(_)) => {
                debug_assert!(false, "an intro reply answered an install command");
            }
            None => {}
        }
    }

    async fn persist(&mut self, bytes: Vec<u8>) -> Result<(), ReachabilityError> {
        let Some(path) = self.persist_file.clone() else {
            return Ok(());
        };
        let Err(err) = store::write_atomic(&path, &bytes) else {
            return Ok(());
        };
        self.observe(ReachabilityEvent::PersistFailed {
            reason: err.to_string(),
        })
        .await
    }
}
