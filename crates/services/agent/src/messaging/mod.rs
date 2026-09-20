//! the collaboration delivery plane: bindings, a durable outbox, and the
//! provider adapters behind them.
//!
//! ## what this owns, and what it refuses to own
//!
//! It owns exactly the local half of delivery: which local session a
//! participant's input goes to, whether the bytes reached a provider, and what
//! the provider said about it. Everything else belongs to the network —
//! admission, authority, ordering, retention — and arrives here already
//! decided. In particular this plane **claims no task and schedules nothing**:
//! a delivery carries a task REFERENCE, which becomes a line in a wrapper and
//! never a claim made on the sender's behalf.
//!
//! ## why it is not part of [`crate::Sessions`]
//!
//! A pty session cannot outlive the link that created it: the node forgets it
//! on disconnect, so a survivor would be an unreachable container. A BINDING is
//! the opposite. The provider session it names is not this daemon's process,
//! was not started by it, and does not die with it — and the whole point of the
//! reconnect case is that a message queued while the node was away is
//! delivered, in sequence, when it comes back. So bindings and the outbox
//! survive a dropped link by construction: they live here, and nothing on this
//! plane is torn down by [`crate::Sessions::close_all`].
//!
//! ## the ordering that makes a crash reportable
//!
//! For every delivery, in this order and no other:
//!
//! 1. [`outbox::Outbox::queued`] — durable, THEN the node is told `Queued`.
//!    Queue ownership is never acknowledged from memory.
//! 2. [`outbox::Outbox::attempting`] — durable, THEN the adapter is called.
//! 3. the adapter's answer — durable, then reported.
//!
//! A crash between 2 and 3 is indistinguishable from success, so it recovers as
//! `DeliveryUnknown` and is never replayed automatically. See [`outbox`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::wire::{self, BindRefusal, Capabilities, State};

pub mod claude;
pub mod codex;
pub mod outbox;
pub mod wrapper;

/// one message, wrapped, on its way to a provider.
pub struct Offer<'a> {
    /// the wrapped text the session will see. Built by [`wrapper::wrap`].
    pub text: &'a str,
    /// the chat message id, for the provider's own correlation.
    pub message_id: &'a str,
    /// whether this may steer an active turn, where that is supported.
    pub urgent: bool,
}

/// what an adapter learned by offering a message.
///
/// The distinction that carries the whole contract is between [`Outcome::Accepted`]
/// — the provider's input interface said so ITSELF — and [`Outcome::Unknown`],
/// which is every case where it did not. A write completing, a socket staying
/// open, or a process exiting 0 where that is not the provider's own acceptance
/// signal all land in the second.
#[derive(Debug)]
pub enum Outcome {
    /// the provider's input interface accepted it, and said so.
    Accepted { reason: Option<String> },
    /// a provider or local approval barrier is holding it.
    Held { reason: String },
    /// the provider refused it, for a nameable reason.
    Refused { reason: String },
    /// the provider reported the message's deadline had passed.
    Expired { reason: String },
    /// no acceptance signal exists, or none arrived. The honest answer.
    Unknown { reason: String },
    /// nothing was offered — the session is closed, the CLI is absent, a turn
    /// is running and this input is not urgent. The message stays `Queued`,
    /// visibly. A closed session is NEVER replaced with a fresh one to make
    /// this go away.
    Deferred { reason: String },
}

impl Outcome {
    /// the delivery state and reason token this outcome records.
    fn record(&self) -> (State, Option<&str>) {
        match self {
            Outcome::Accepted { reason } => (State::AdapterAccepted, reason.as_deref()),
            Outcome::Held { reason } => (State::Held, Some(reason)),
            Outcome::Refused { reason } => (State::Refused, Some(reason)),
            Outcome::Expired { reason } => (State::Expired, Some(reason)),
            Outcome::Unknown { reason } => (State::DeliveryUnknown, Some(reason)),
            // still queued: nothing was offered, so nothing changed except
            // what we can say about why.
            Outcome::Deferred { reason } => (State::Queued, Some(reason)),
        }
    }
}

/// which local provider session a device label names.
///
/// This lives ON THE DEVICE and never on a wire. The node names a `device`; the
/// map from that label to a session is read from local configuration by the
/// process that can actually reach the session. So a provider session id, a
/// thread id and a socket path are facts of one machine, exactly as the spec
/// requires — the network never learns any of them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    /// an existing Claude Code session, by its immutable session id.
    ClaudeSession { session_id: String },
    /// an existing Codex session, driven through the installed `codex queue`.
    CodexThread { thread_id: String },
    /// a Codex thread this service manages over a local App Server.
    CodexManaged { thread_id: String },
}

/// the operator's local attachment map: `device label -> target`.
///
/// Re-read on every bind rather than cached, so attaching a session is an edit
/// to a file the operator owns and not a daemon restart.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachments {
    #[serde(default)]
    pub devices: HashMap<String, Target>,
}

impl Attachments {
    /// read the map, treating an absent file as an empty one — a daemon with
    /// no attachments is an ordinary state, not a misconfiguration.
    pub async fn load(path: &Path) -> Result<Self, String> {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("read attachments: {error}")),
        };
        serde_json::from_str(&text).map_err(|error| format!("parse attachments: {error}"))
    }
}

/// one participant's input on one conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BindingKey {
    conversation: String,
    participant: String,
}

/// a live binding: its generation, and the lane its deliveries ride.
///
/// The adapter itself is NOT here. It lives in the lane's drain task, which is
/// the only thing that calls it — so an adapter call, which spawns a process or
/// writes a socket, never happens while this map is locked, and one slow
/// provider cannot stall every other binding on the device.
struct Bound {
    generation: u64,
    /// this binding's ordered delivery lane.
    ///
    /// One task drains it, so a binding's eligible messages reach its provider
    /// IN SEQUENCE — which is the contract, and which concurrent per-delivery
    /// tasks quietly break: two offers racing means the answer to a question
    /// can be written into a session before the question is. Bounded, so a
    /// node that pushes faster than a provider accepts applies back-pressure
    /// instead of growing without limit; per binding, so one stalled provider
    /// cannot hold up another's deliveries.
    lane: mpsc::Sender<Box<wire::Deliver>>,
}

/// how many deliveries may wait on one binding's lane.
///
/// Small on purpose. The network holds the real mailbox (256 undelivered per
/// participant); this is only the hand-off in front of one provider, and a
/// deep local queue would just move the network's accounting somewhere it
/// cannot see it.
const LANE_CAPACITY: usize = 32;

/// how many commands may wait on the plane's own lane before it refuses.
///
/// Deeper than a binding's lane because it carries binds, unbinds and clock
/// updates as well as deliveries, and a bind can be slow (it resolves a
/// session, and may start an App Server child).
const COMMAND_LANE: usize = 256;

/// the provider behind a binding.
enum Adapter {
    Claude(claude::ClaudeInbox),
    CodexQueue(codex::CodexQueue),
    CodexManaged(codex::CodexAppServer),
}

impl Adapter {
    fn capabilities(&self) -> Capabilities {
        match self {
            Adapter::Claude(inbox) => inbox.capabilities(),
            Adapter::CodexQueue(queue) => queue.capabilities(),
            Adapter::CodexManaged(managed) => managed.capabilities(),
        }
    }

    /// offer one message, and say whether the outcome can still change.
    ///
    /// The second half is `Some` only for a HELD message on an adapter that can
    /// hear it released. The two Codex adapters answer `None` on every path:
    /// neither surfaces a hold, so neither has a release to report.
    async fn offer(&self, offer: &Offer<'_>) -> (Outcome, Option<claude::Follow>) {
        match self {
            Adapter::Claude(inbox) => inbox.offer(offer).await,
            Adapter::CodexQueue(queue) => (queue.offer(offer).await, None),
            Adapter::CodexManaged(managed) => (managed.offer(offer).await, None),
        }
    }
}

/// the delivery plane. Arc-backed so a clone rides into each delivery task.
#[derive(Clone)]
pub struct Deliveries(Arc<Inner>);

struct Inner {
    /// this plane's ordered command lane. See [`Deliveries::enqueue`].
    commands: mpsc::Sender<Messaging>,
    /// the operator's local device map.
    attachments: PathBuf,
    /// where Claude publishes its session registry.
    registry: PathBuf,
    /// the Codex executable to run, for both the queue CLI and the App Server.
    codex: PathBuf,
    /// the shared App Server child, started on the first managed binding.
    app_server: tokio::sync::OnceCell<Arc<codex::Rpc>>,
    outbox: outbox::Outbox,
    /// the receipt address, when one could be bound. `None` means every Claude
    /// delivery settles `DeliveryUnknown` — degraded, never fabricated.
    receipts: Option<Arc<claude::Receipts>>,
    bindings: Mutex<HashMap<BindingKey, Bound>>,
    /// the freshest agreed network clock the node has told us about. Written
    /// only by [`wire::Command::MsgTime`]; this daemon never sets it from a
    /// clock of its own.
    network_now: std::sync::atomic::AtomicU64,
    events: mpsc::Sender<wire::Event>,
}

/// the canonical digest of one message's identity, hex.
///
/// What it covers is the REQUEST: the sender, the id, the kind, the body, the
/// references, the task and the deadline. What it deliberately excludes is
/// everything about this particular attempt — `binding_generation`,
/// `network_now` — because a retry to a re-bound device at a later moment is
/// the SAME message, and treating it as a conflict would refuse exactly the
/// retry the contract asks for.
fn digest_of(deliver: &wire::Deliver) -> String {
    use sha2::{Digest as _, Sha256};
    let canonical = serde_json::json!({
        "conversation": deliver.conversation,
        "participant": deliver.participant,
        "seq": deliver.seq,
        "message_id": deliver.message_id,
        "sender": deliver.sender,
        "kind": deliver.kind,
        "task": deliver.task,
        "reply_to": deliver.reply_to,
        "body": deliver.body,
        "references": deliver.references,
        "expires_at": deliver.expires_at,
        "urgent": deliver.urgent,
    });
    format!("{:x}", Sha256::digest(canonical.to_string().as_bytes()))
}

impl Deliveries {
    /// build the plane, recovering whatever the last run left behind.
    ///
    /// Recovered items are REPORTED and never re-driven: an item that recovers
    /// as `DeliveryUnknown` may already have been read by a model, and this
    /// daemon no longer holds its body to replay even if it wanted to.
    pub async fn open(
        storage: &Path,
        network: &str,
        attachments: PathBuf,
        registry: PathBuf,
        codex: PathBuf,
        receipts: Option<Arc<claude::Receipts>>,
        events: mpsc::Sender<wire::Event>,
    ) -> Result<Self, String> {
        let (outbox, recovered) = outbox::Outbox::open(storage, network).await?;
        let (commands, queue) = mpsc::channel(COMMAND_LANE);
        let plane = Self(Arc::new(Inner {
            commands,
            attachments,
            registry,
            codex,
            app_server: tokio::sync::OnceCell::new(),
            outbox,
            receipts,
            bindings: Mutex::new(HashMap::new()),
            network_now: std::sync::atomic::AtomicU64::new(0),
            events,
        }));
        plane.spawn_command_lane(queue);
        for (key, entry) in recovered {
            tracing::info!(
                target: "ducktape::collab",
                conversation = %key.conversation,
                seq = key.seq,
                state = entry.state.token(),
                reason = entry.reason.as_deref().unwrap_or("none"),
                "recovered a delivery from the outbox"
            );
            plane
                .0
                .emit(wire::Event::MsgDelivery {
                    conversation: key.conversation,
                    participant: entry.participant,
                    seq: key.seq,
                    binding_generation: entry.binding_generation,
                    sender: entry.sender,
                    message_id: entry.message_id,
                    state: entry.state,
                    reason: entry.reason,
                })
                .await;
        }
        Ok(plane)
    }

    /// hand one command to this plane's ordered command lane.
    ///
    /// Non-blocking by construction, and ordered: the link must keep reading
    /// (a bind starts a child process, a deliver fsyncs), while a `MsgDeliver`
    /// that overtook its `MsgBind` — or another deliver — would put a
    /// conversation's messages into a provider out of sequence. One lane, one
    /// drain task, `try_send`: the link never waits, and nothing reorders.
    ///
    /// `false` when the lane is full. The command is DROPPED, never buffered
    /// past the ceiling; the network still holds the message and will re-drive
    /// it.
    pub fn enqueue(&self, command: Messaging) -> bool {
        match self.0.commands.try_send(command) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    target: "ducktape::collab",
                    reason = "command_lane_full",
                    "a collaboration command was dropped: this daemon is behind"
                );
                false
            }
            // the drain task is gone, which happens only as the process ends.
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// THE dispatch. One arm per command, each a single delegation to a
    /// handler named for it, so a new command fails the build until it is
    /// routed.
    pub async fn dispatch(&self, command: Messaging) {
        match command {
            Messaging::Bind(bind) => self.bind(bind).await,
            Messaging::Unbind {
                conversation,
                participant,
                generation,
            } => self.unbind(&conversation, &participant, generation),
            Messaging::Deliver(deliver) => self.deliver(deliver).await,
            Messaging::Time { network_now } => self.advance_clock(network_now).await,
            Messaging::Replay {
                conversation,
                participant,
            } => self.replay(&conversation, &participant).await,
        }
    }

    /// remember the agreed clock the node reports, and retire what it has
    /// aged out.
    ///
    /// Monotonic by construction: a lower value than one already seen is
    /// ignored rather than applied, so a reordered or replayed frame cannot
    /// move this device's notion of network time backwards and revive an
    /// expired message — or un-retire a record it already dropped.
    ///
    /// The deadline is what prunes the dedup record, and it is a NETWORK fact:
    /// past `expires_at` the module's own delivery record is gone, so a replay
    /// of that message cannot be admitted upstream and forgetting it here costs
    /// nothing. Nothing is reported to the node — pruning a record for a
    /// message nobody may re-drive changes no delivery's state, and inventing a
    /// receipt for it would be a status this daemon does not have.
    async fn advance_clock(&self, network_now: u64) {
        let previous = self
            .0
            .network_now
            .fetch_max(network_now, std::sync::atomic::Ordering::SeqCst);
        let advanced = network_now > previous;
        if !advanced {
            return;
        }
        match self.0.outbox.retire_expired(network_now).await {
            Ok(0) => {}
            Ok(pruned) => tracing::debug!(
                target: "ducktape::collab",
                network_now,
                pruned,
                "retired outbox records whose deadline has passed"
            ),
            Err(error) => tracing::warn!(
                target: "ducktape::collab",
                network_now,
                reason = "outbox_compact_failed",
                %error,
                "the outbox could not be compacted; it keeps every record it has"
            ),
        }
    }

    /// Re-report every delivery state this daemon durably holds for one
    /// binding, oldest sequence first.
    ///
    /// A delivery state is a fact only this process observed. The node cannot
    /// re-derive one and nothing else re-sends it, so a receipt lost on the way
    /// there is lost for good — the network reads `Queued` forever for a
    /// message a provider took. This is how it is recovered, and it reports
    /// what is ON DISK rather than what anyone remembers.
    ///
    /// It offers nothing to a provider, claims nothing and writes nothing. A
    /// state already committed upstream is refused there as a transition
    /// already made, so replaying more than was lost costs a refused op and
    /// never a duplicated instruction.
    async fn replay(&self, conversation: &str, participant: &str) {
        let tracked = match self.0.outbox.tracked(conversation, participant).await {
            Ok(tracked) => tracked,
            Err(error) => {
                tracing::warn!(
                    target: "ducktape::collab",
                    conversation = %conversation,
                    reason = "outbox_unreadable",
                    %error,
                    "cannot replay delivery states"
                );
                return;
            }
        };
        tracing::info!(
            target: "ducktape::collab",
            conversation = %conversation,
            participant = %participant,
            items = tracked.len(),
            "replaying durable delivery states"
        );
        for (seq, entry) in tracked {
            self.0
                .emit(wire::Event::MsgDelivery {
                    conversation: conversation.to_string(),
                    participant: entry.participant.clone(),
                    seq,
                    binding_generation: entry.binding_generation,
                    sender: entry.sender.clone(),
                    message_id: entry.message_id.clone(),
                    state: entry.state,
                    reason: entry.reason.clone(),
                })
                .await;
        }
    }

    /// how many bindings are live — what the daemon's status line reports.
    pub fn bound(&self) -> usize {
        self.0
            .bindings
            .lock()
            .expect("collab bindings lock poisoned")
            .len()
    }

    // ---- the handlers -----------------------------------------------------

    async fn bind(&self, bind: wire::Bind) {
        let event = match self.try_bind(&bind).await {
            Ok(capabilities) => {
                tracing::info!(
                    target: "ducktape::collab",
                    conversation = %bind.conversation,
                    participant = %bind.participant,
                    generation = bind.generation,
                    "binding attached"
                );
                wire::Event::MsgBound {
                    conversation: bind.conversation,
                    participant: bind.participant,
                    generation: bind.generation,
                    capabilities,
                }
            }
            Err(reason) => {
                tracing::warn!(
                    target: "ducktape::collab",
                    conversation = %bind.conversation,
                    participant = %bind.participant,
                    generation = bind.generation,
                    reason = reason.token(),
                    "bind refused"
                );
                wire::Event::MsgBindRefused {
                    conversation: bind.conversation,
                    participant: bind.participant,
                    generation: bind.generation,
                    reason,
                }
            }
        };
        self.0.emit(event).await;
    }

    /// resolve and install one binding. Returns the refusal rather than
    /// emitting it, so the whole admission ladder is testable without a wire.
    async fn try_bind(&self, bind: &wire::Bind) -> Result<Capabilities, BindRefusal> {
        let key = BindingKey {
            conversation: bind.conversation.clone(),
            participant: bind.participant.clone(),
        };
        // fenced FIRST, before any resolution work: two devices cannot both
        // claim one participant's input, and the one that arrives with a
        // generation the current holder has already reached is the loser —
        // including a device returning from a disconnect with its old number.
        {
            let bindings = self
                .0
                .bindings
                .lock()
                .expect("collab bindings lock poisoned");
            let stale = bindings
                .get(&key)
                .is_some_and(|bound| bind.generation <= bound.generation);
            if stale {
                return Err(BindRefusal::StaleGeneration);
            }
        }
        let attachments = Attachments::load(&self.0.attachments)
            .await
            .map_err(|_| BindRefusal::UnknownDevice)?;
        let target = attachments
            .devices
            .get(&bind.device)
            .ok_or(BindRefusal::UnknownDevice)?;
        let adapter = self.adapter_for(target).await?;
        let capabilities = adapter.capabilities();
        let (lane, queue) = mpsc::channel(LANE_CAPACITY);
        // the lane's ONLY long-lived sender lives in the map, so replacing or
        // releasing this binding ends its drain task by dropping the entry —
        // the same drop-driven teardown the terminal plane's driver takes.
        self.spawn_lane(bind.generation, adapter, queue);
        self.0
            .bindings
            .lock()
            .expect("collab bindings lock poisoned")
            .insert(
                key,
                Bound {
                    generation: bind.generation,
                    lane,
                },
            );
        Ok(capabilities)
    }

    /// drain the plane's command lane, in arrival order, one at a time.
    fn spawn_command_lane(&self, mut queue: mpsc::Receiver<Messaging>) {
        let plane = self.clone();
        tokio::spawn(async move {
            while let Some(command) = queue.recv().await {
                plane.dispatch(command).await;
            }
        });
    }

    /// drain one binding's lane, in order, one delivery at a time.
    fn spawn_lane(
        &self,
        generation: u64,
        adapter: Arc<Adapter>,
        mut queue: mpsc::Receiver<Box<wire::Deliver>>,
    ) {
        let plane = self.clone();
        tokio::spawn(async move {
            while let Some(deliver) = queue.recv().await {
                plane.perform(&deliver, generation, &adapter).await;
            }
        });
    }

    /// build the adapter one target names.
    async fn adapter_for(&self, target: &Target) -> Result<Arc<Adapter>, BindRefusal> {
        let adapter = match target {
            Target::ClaudeSession { session_id } => {
                let attached = claude::resolve(&self.0.registry, session_id)
                    .await
                    .map_err(|unreachable| {
                        tracing::warn!(
                            target: "ducktape::collab",
                            reason = unreachable.token(),
                            "a bound claude session could not be reached"
                        );
                        BindRefusal::SessionUnreachable
                    })?;
                // a verdict is only trusted from the process this binding
                // actually names — registered here, where the binding is what
                // authorizes it, and nowhere else.
                if let Some(receipts) = &self.0.receipts {
                    receipts.trust(attached.pid);
                }
                Adapter::Claude(claude::ClaudeInbox::new(attached, self.0.receipts.clone()))
            }
            Target::CodexThread { thread_id } => Adapter::CodexQueue(codex::CodexQueue::new(
                thread_id.clone(),
                self.0.codex.clone(),
            )),
            Target::CodexManaged { thread_id } => Adapter::CodexManaged(
                codex::CodexAppServer::new(thread_id.clone(), self.app_server().await?),
            ),
        };
        Ok(Arc::new(adapter))
    }

    /// the one App Server child this daemon manages, started on first use.
    ///
    /// One child, not one per binding: an App Server hosts many threads, and a
    /// process per managed session would be a second scheduler's worth of
    /// lifetime to own for no gain. It is started lazily so a daemon with no
    /// managed bindings never spawns one at all.
    async fn app_server(&self) -> Result<Arc<codex::Rpc>, BindRefusal> {
        self.0
            .app_server
            .get_or_try_init(|| async {
                codex::Rpc::start(&self.0.codex).await.map_err(|error| {
                    tracing::warn!(
                        target: "ducktape::collab",
                        reason = "app_server_unavailable",
                        %error,
                        "a managed codex binding needs a local app server"
                    );
                    BindRefusal::SessionUnreachable
                })
            })
            .await
            .cloned()
    }

    /// release a binding, if the caller names the generation actually held.
    ///
    /// A stale generation cannot detach: a device returning from a disconnect
    /// must not be able to unbind the attachment that replaced it.
    fn unbind(&self, conversation: &str, participant: &str, generation: u64) {
        let key = BindingKey {
            conversation: conversation.to_string(),
            participant: participant.to_string(),
        };
        let mut bindings = self
            .0
            .bindings
            .lock()
            .expect("collab bindings lock poisoned");
        let current = bindings.get(&key).map(|bound| bound.generation);
        if current != Some(generation) {
            tracing::warn!(
                target: "ducktape::collab",
                conversation = %conversation,
                participant = %participant,
                generation,
                reason = "stale_generation",
                "unbind refused"
            );
            return;
        }
        bindings.remove(&key);
        tracing::info!(
            target: "ducktape::collab",
            conversation = %conversation,
            participant = %participant,
            generation,
            "binding released"
        );
    }

    /// take ownership of one message and put it on its binding's lane.
    ///
    /// Everything slow happens on the LANE, not here: this decides whether the
    /// message is ours, whether we have seen it before, and records the
    /// ownership — then hands it over in order.
    async fn deliver(&self, deliver: Box<wire::Deliver>) {
        let key = outbox::Key {
            conversation: deliver.conversation.clone(),
            seq: deliver.seq,
        };

        // a delivery aimed at a generation this device does not hold is not
        // ours to perform. Taking ownership of it would let a stale target's
        // traffic occupy the current attachment's queue.
        let Some((generation, lane)) = self.lane_for(&deliver) else {
            tracing::warn!(
                target: "ducktape::collab",
                conversation = %deliver.conversation,
                seq = deliver.seq,
                reason = "stale_binding_generation",
                "delivery not accepted: it names a binding this device does not hold"
            );
            return;
        };
        // a permit BEFORE ownership: taking a message we then cannot hand to
        // its lane would leave it owned and unprocessed. A full lane is a
        // retryable capacity condition, and refusing is what makes it one.
        let Ok(permit) = lane.try_reserve() else {
            tracing::warn!(
                target: "ducktape::collab",
                conversation = %deliver.conversation,
                seq = deliver.seq,
                reason = "lane_full",
                "delivery not accepted: this binding's lane is full"
            );
            return;
        };

        let entry = outbox::Entry {
            participant: deliver.participant.clone(),
            binding_generation: deliver.binding_generation,
            sender: deliver.sender.clone(),
            message_id: deliver.message_id.clone(),
            expires_at: deliver.expires_at,
            digest: digest_of(&deliver),
            state: State::Queued,
            reason: None,
            claimed: false,
        };
        // durable BEFORE the node hears `Queued`, and atomic against a
        // duplicate: queue ownership is a promise that survives this process,
        // so it is written down before it is made — and made only once.
        let admission = match self.0.outbox.admit(&key, &entry).await {
            Ok(admission) => admission,
            Err(error) => {
                tracing::error!(
                    target: "ducktape::collab",
                    conversation = %deliver.conversation,
                    seq = deliver.seq,
                    reason = "outbox_admit_failed",
                    %error,
                    "delivery not accepted: the outbox could not record it"
                );
                return;
            }
        };
        match admission {
            // seen before, byte for byte. Report what already happened and
            // offer NOTHING: re-offering an accepted or unknown instruction is
            // the duplicate execution the retry contract exists to prevent.
            outbox::Admission::Duplicate { state, reason } => {
                tracing::debug!(
                    target: "ducktape::collab",
                    conversation = %deliver.conversation,
                    seq = deliver.seq,
                    state = state.token(),
                    reason = "duplicate_retry",
                    "answering a retry from the record"
                );
                self.report(&deliver, generation, state, reason.as_deref())
                    .await;
            }
            // one id, one message.
            outbox::Admission::Conflict => {
                tracing::warn!(
                    target: "ducktape::collab",
                    conversation = %deliver.conversation,
                    seq = deliver.seq,
                    reason = "message_id_conflict",
                    "refused: a different message already holds this id"
                );
                self.report(
                    &deliver,
                    generation,
                    State::Refused,
                    Some("message_id_conflict"),
                )
                .await;
            }
            // `Fresh` is a new item; `Reclaimed` is one the journal still
            // records as never offered — a crash between taking ownership and
            // reaching the lane, or a provider that told us it never wrote.
            // Both are safe to offer, and both are safe for the same reason:
            // `Queued` is the one state that proves no provider has seen it.
            outbox::Admission::Fresh | outbox::Admission::Reclaimed => {
                self.report(&deliver, generation, State::Queued, None).await;
                permit.send(deliver);
            }
        }
    }

    /// offer one owned message to its provider. Runs on the binding's lane, so
    /// this binding's messages reach the provider one at a time, in order.
    async fn perform(&self, deliver: &wire::Deliver, generation: u64, adapter: &Adapter) {
        let key = outbox::Key {
            conversation: deliver.conversation.clone(),
            seq: deliver.seq,
        };
        // the binding may have been replaced while this waited its turn — a
        // held item ahead of it, a slow provider, an operator re-attaching.
        // Offering it now would write a message aimed at one attachment into
        // the session that replaced it, which is precisely the stale-target
        // delivery the generation exists to prevent. Re-checked HERE because
        // the lane is where the waiting happens.
        let still_current = self.holds_generation(deliver, generation);
        if !still_current {
            self.not_offered(&key, deliver, generation, "binding_replaced")
                .await;
            return;
        }
        // expiry is decided against the AGREED clock, never against this
        // laptop's — and against the FRESHEST agreed value known, not the one
        // frozen at admission: a message can sit on this lane while a provider
        // is slow, and a deadline that passed in the meantime has passed.
        let now = self.network_now(deliver.network_now);
        let expired = now >= deliver.expires_at;
        if expired {
            self.settle(
                &key,
                deliver,
                generation,
                State::Expired,
                Some("deadline_passed"),
            )
            .await;
            return;
        }

        let text = wrapper::wrap(deliver);
        let offer = Offer {
            text: &text,
            message_id: &deliver.message_id,
            urgent: deliver.urgent,
        };

        // durable BEFORE the provider is touched. Everything after this line
        // is unverifiable after a crash, which is exactly what the record says.
        if let Err(error) = self.0.outbox.attempting(&key).await {
            tracing::error!(
                target: "ducktape::collab",
                conversation = %deliver.conversation,
                seq = deliver.seq,
                reason = "outbox_write_failed",
                %error,
                "delivery left queued: the attempt could not be recorded"
            );
            return;
        }
        let (outcome, follow) = adapter.offer(&offer).await;
        // `Deferred` is the adapter's own statement that it wrote NOTHING —
        // and it is the only thing entitled to make that statement, because
        // `attempting` is already on the disk. Recorded through its own seam
        // so the item goes back to deliverable, rather than through `settle`,
        // which refuses to walk an uncertain delivery back to `Queued`.
        if let Outcome::Deferred { reason } = &outcome {
            self.not_offered(&key, deliver, generation, reason).await;
            return;
        }
        let (state, reason) = outcome.record();
        self.settle(&key, deliver, generation, state, reason).await;
        if let Some(follow) = follow {
            self.watch_hold(follow, key, deliver.clone(), generation);
        }
    }

    /// Follow a held message to its resolution, OFF this binding's lane.
    ///
    /// Off it deliberately: the lane is ordered and one delivery deep, and a
    /// hold waits on a person. Blocking here would stop every message behind
    /// this one until somebody clicked approve.
    ///
    /// The record is already `Held` and already reported. This adds the SECOND
    /// observation the module's diagram admits (`Held -> AdapterAccepted |
    /// Refused | Expired`) and nothing else: a window that passes says nothing
    /// and settles nothing.
    fn watch_hold(
        &self,
        follow: claude::Follow,
        key: outbox::Key,
        deliver: wire::Deliver,
        generation: u64,
    ) {
        let plane = self.clone();
        tokio::spawn(async move {
            let Some(outcome) = follow.resolve().await else {
                tracing::debug!(
                    target: "ducktape::collab",
                    conversation = %deliver.conversation,
                    seq = deliver.seq,
                    reason = "hold_unresolved",
                    "a held delivery was never resolved; it stays held"
                );
                return;
            };
            // the generation is re-checked because a hold outlives a bind: the
            // participant may have moved to another device while a human was
            // deciding, and the module refuses a receipt from a replaced
            // attachment anyway. Refusing here keeps the reason local.
            if !plane.holds_generation(&deliver, generation) {
                tracing::debug!(
                    target: "ducktape::collab",
                    conversation = %deliver.conversation,
                    seq = deliver.seq,
                    reason = "binding_replaced",
                    "a hold resolved after its binding was replaced"
                );
                return;
            }
            let (state, reason) = outcome.record();
            plane
                .settle(&key, &deliver, generation, state, reason)
                .await;
        });
    }

    /// whether this device still holds the generation a queued delivery was
    /// admitted under.
    fn holds_generation(&self, deliver: &wire::Deliver, generation: u64) -> bool {
        let key = BindingKey {
            conversation: deliver.conversation.clone(),
            participant: deliver.participant.clone(),
        };
        self.0
            .bindings
            .lock()
            .expect("collab bindings lock poisoned")
            .get(&key)
            .is_some_and(|bound| bound.generation == generation)
    }

    /// record that no provider was offered this item, and report it still
    /// queued.
    async fn not_offered(
        &self,
        key: &outbox::Key,
        deliver: &wire::Deliver,
        generation: u64,
        reason: &str,
    ) {
        if let Err(error) = self.0.outbox.not_offered(key, reason).await {
            tracing::error!(
                target: "ducktape::collab",
                conversation = %deliver.conversation,
                seq = deliver.seq,
                reason = "outbox_write_failed",
                %error,
                "a delivery could not be returned to the queue"
            );
        }
        tracing::debug!(
            target: "ducktape::collab",
            conversation = %deliver.conversation,
            seq = deliver.seq,
            reason,
            "delivery left queued: no provider was offered it"
        );
        self.report(deliver, generation, State::Queued, Some(reason))
            .await;
    }

    /// the binding's generation and lane, if this device holds the one the
    /// delivery names.
    fn lane_for(&self, deliver: &wire::Deliver) -> Option<(u64, mpsc::Sender<Box<wire::Deliver>>)> {
        let key = BindingKey {
            conversation: deliver.conversation.clone(),
            participant: deliver.participant.clone(),
        };
        let bindings = self
            .0
            .bindings
            .lock()
            .expect("collab bindings lock poisoned");
        let bound = bindings.get(&key)?;
        (bound.generation == deliver.binding_generation)
            .then(|| (bound.generation, bound.lane.clone()))
    }

    /// the freshest agreed network clock this daemon knows.
    ///
    /// `admitted` is the value the frame carried; the node also pushes the
    /// current one as it advances. The larger wins — this daemon owns no clock
    /// and invents no time, it only remembers the newest one it was told.
    fn network_now(&self, admitted: u64) -> u64 {
        admitted.max(self.0.network_now.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// record an outcome durably, then report it.
    async fn settle(
        &self,
        key: &outbox::Key,
        deliver: &wire::Deliver,
        generation: u64,
        state: State,
        reason: Option<&str>,
    ) {
        if let Err(error) = self.0.outbox.settled(key, state, reason).await {
            tracing::error!(
                target: "ducktape::collab",
                conversation = %deliver.conversation,
                seq = deliver.seq,
                reason = "outbox_write_failed",
                %error,
                "a delivery outcome could not be recorded"
            );
        }
        tracing::debug!(
            target: "ducktape::collab",
            conversation = %deliver.conversation,
            seq = deliver.seq,
            state = state.token(),
            reason = reason.unwrap_or("none"),
            "delivery settled"
        );
        self.report(deliver, generation, state, reason).await;
    }

    /// tell the node one delivery state.
    async fn report(
        &self,
        deliver: &wire::Deliver,
        generation: u64,
        state: State,
        reason: Option<&str>,
    ) {
        self.0
            .emit(wire::Event::MsgDelivery {
                conversation: deliver.conversation.clone(),
                participant: deliver.participant.clone(),
                seq: deliver.seq,
                binding_generation: generation,
                sender: deliver.sender.clone(),
                message_id: deliver.message_id.clone(),
                state,
                reason: reason.map(str::to_string),
            })
            .await;
    }
}

/// the messaging half of [`wire::Command`], routed here rather than through
/// [`crate::Sessions`] — the two planes share a link, not a lifetime.
pub enum Messaging {
    Bind(wire::Bind),
    Unbind {
        conversation: String,
        participant: String,
        generation: u64,
    },
    /// boxed: a `Deliver` carries a body, and an enum is as large as its
    /// largest variant on every lane it rides.
    Deliver(Box<wire::Deliver>),
    /// the agreed network clock has advanced.
    Time {
        network_now: u64,
    },
    /// re-report every durable delivery state for one binding.
    Replay {
        conversation: String,
        participant: String,
    },
}

/// route one command to its plane.
///
/// THE match over every [`wire::Command`] variant, and the only one — a new
/// command fails the build here until it is routed, which is the whole reason
/// this is a function and not a `_` arm at each call site. The terminal half
/// is handed back untouched for [`crate::Sessions`], because the two planes
/// share a link and nothing else: a pty dies with that link and a binding
/// outlives it.
#[allow(
    clippy::result_large_err,
    reason = "the Err IS the input, handed straight back for the terminal plane \
              to run — boxing it would allocate on every keystroke"
)]
pub fn route(command: wire::Command) -> Result<Messaging, wire::Command> {
    match command {
        wire::Command::MsgBind(bind) => Ok(Messaging::Bind(bind)),
        wire::Command::MsgUnbind {
            conversation,
            participant,
            generation,
        } => Ok(Messaging::Unbind {
            conversation,
            participant,
            generation,
        }),
        wire::Command::MsgDeliver(deliver) => Ok(Messaging::Deliver(deliver)),
        wire::Command::MsgTime { network_now } => Ok(Messaging::Time { network_now }),
        wire::Command::MsgReplay {
            conversation,
            participant,
        } => Ok(Messaging::Replay {
            conversation,
            participant,
        }),
        terminal @ (wire::Command::TermCreate(_)
        | wire::Command::TermInput { .. }
        | wire::Command::TermResize { .. }
        | wire::Command::TermClose { .. }) => Err(terminal),
    }
}

impl Inner {
    /// the ONE writer, mirroring the terminal plane's.
    async fn emit(&self, event: wire::Event) {
        if self.events.send(event).await.is_err() {
            tracing::debug!(
                target: "ducktape::collab",
                reason = "link_closed",
                "collab event dropped"
            );
        }
    }
}

#[cfg(test)]
mod tests;
