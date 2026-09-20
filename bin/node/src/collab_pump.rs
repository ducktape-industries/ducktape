//! The node's collaboration half: committed state out to the agent daemon,
//! the daemon's receipts back into committed state.
//!
//! The daemon owns no chain and the module owns no pty, so something has to
//! stand between them. That is this, and it is deliberately the only place
//! either direction crosses:
//!
//! ```text
//!   collaboration module ──read──> pump ──wire::Command──> agent daemon
//!   collaboration module <─Acknowledge─ pump <─wire::Event─ agent daemon
//! ```
//!
//! ## every read is authenticated as the binding, not as the node
//!
//! Reads go out on [`noded::NodeCommand::QueryAs`] carrying the binding's
//! SCOPED SERVICE KEY as the reader, never the node's own identity. The module
//! refuses `Origin::System` for all of `ProtectedRead`, so a node cannot read a
//! channel's delivery records it holds no binding on — including the ones its
//! own operator owns. `via` names the channel the key is scoped to, because
//! the key store is hashed and the module cannot find the binding by scanning.
//!
//! The BODY is chat's. A delivery record names a chat message by id, and the
//! pump reads that message over the node's public query lane: a chat message
//! is replicated committed state that every validator holds in plaintext, so
//! nothing about it is protected by the collaboration module.
//!
//! ## the deadline is the network's, and it is asked immediately before
//!
//! Nothing here consults a wall clock. A delivery window is checked with
//! `ProtectedRead::DeliveryEligibility` — judged against the block's
//! `consensus_time` — in the same sweep that hands the message over, so a
//! message that expired while an earlier one was being read is never delivered
//! under a verdict taken before it. A historical `adapter_accepted` in the
//! receipt is a FACT about what happened, and the module is explicit that it is
//! not permission to deliver again; the pump asks eligibility and never reads a
//! receipt to decide.
//!
//! ## what is never replayed
//!
//! Only a `Stored` record is handed over. `Queued` means the daemon's durable
//! journal already owns it, `Held` means a provider is holding it, and
//! `DeliveryUnknown` means nobody can establish whether input was accepted —
//! the spec forbids replaying that automatically, and the module answers
//! `NotReplayable` so the pump cannot even by mistake.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::wire::{chat, collaboration as collab};
use agent_service::wire;
use futures::SinkExt as _;
use futures::channel::{mpsc, oneshot};
use tokio::sync::mpsc as lane;

use crate::collab_keys::Attached;

/// The module every read and write here names.
const COLLABORATION: &str = "collaboration";

/// Events read per page. The module caps a page itself; this is the pump's own
/// ceiling on how much one channel may hold the sweep for.
const PAGE: u64 = 64;

/// Pages one channel may consume in one sweep. A channel catching up over
/// thousands of events must not starve the ones behind it — the cursor
/// persists across sweeps, so it resumes exactly where this stopped.
const PAGES_PER_SWEEP: usize = 8;

/// How deep the daemon's receipts may queue before the terminal plane starts
/// dropping them ([`noded::ServiceLink::route_collab_to`]). Each one costs
/// a chain submission, so this is a few blocks of head room and not a buffer to
/// hide a wedged pump behind.
const RECEIPT_LANE: usize = 256;

/// How many MESSAGES may owe the chain a receipt before the pump stops handing
/// NEW ones to the daemon.
///
/// Backpressure and not a bin. Nothing owed is ever dropped: the diagram refuses
/// `Stored -> AdapterAccepted`, so a lost `Queued` makes the acceptance behind
/// it permanently unsubmittable, and a later state cannot stand in for an
/// earlier one. The pressure is applied at the only point where there is still
/// a choice — a delivery not yet made — because by the time a RECEIPT arrives
/// the daemon has already spent the fact and nobody would re-report it.
const MAX_OWING_MESSAGES: usize = 4096;

/// How often production sweeps committed state.
///
/// The sweep is what notices mail; the receipts come back on their own lane and
/// are never waited for here. It is a POLL because the module's sibling-module
/// notification (`CollaborationEvent::ChannelAdvanced`) reaches modules,
/// not the node process — so there is no push to subscribe to from out here.
// ponytail: a poll, because nothing pushes to this process yet. When the node
// grows a committed-event subscription, feed `wake` from it and keep this as
// the floor.
const SWEEP: std::time::Duration = std::time::Duration::from_secs(5);

/// Wire the pump onto a running node and let it run until the node stops.
///
/// Nothing is spawned when there is no terminal plane to pump for, when the
/// node serves no chain, or when the plane already has a collaboration
/// consumer — one pump per node, for the reason
/// [`noded::ServiceLink::route_collab_to`] gives. Each is an ordinary
/// state and each says which one it was at `info`, once per boot.
pub(crate) fn spawn(
    commands: mpsc::Sender<noded::NodeCommand>,
    status: noded::StatusCell,
    terminals: Option<&noded::ServiceLink>,
    workspace: PathBuf,
    network: &str,
) -> bool {
    let Some(terminals) = terminals else {
        return refuse_to_start("no_terminal_plane");
    };
    // an empty chain id is what a daemon serving no chain reports, and it is
    // also what scopes a service key (`collab_keys`): scoping to it would put
    // every network's bindings in one namespace. Nothing to pump, and nothing
    // safe to sign with.
    if network.is_empty() {
        return refuse_to_start("no_network");
    }
    let (receipt_tx, receipt_rx) = lane::channel(RECEIPT_LANE);
    if !terminals.route_collab_to(receipt_tx) {
        return refuse_to_start("collab_lane_taken");
    }
    let (wake_tx, wake_rx) = lane::channel(1);
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(SWEEP);
        // a sweep that outruns the interval must not queue a backlog of them:
        // each one reads the same committed state, so a skipped tick costs
        // nothing and a queued one costs a whole duplicate pass.
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            // the lane holds ONE tick. A full lane means a sweep is still
            // running and another is already queued behind it — dropping this
            // one is the same "skip" the interval does.
            if wake_tx.try_send(()).is_err() && wake_tx.is_closed() {
                return;
            }
        }
    });
    let pump = Pump::new(
        commands,
        status,
        terminals.clone(),
        workspace,
        network.to_string(),
    );
    tokio::spawn(pump.run(receipt_rx, wake_rx));
    tracing::info!(target: "ducktape::collab", "collaboration_pump_ready");
    true
}

/// this node runs no collaboration pump, and which of the three reasons it is.
/// One line per boot, so an operator whose messages go nowhere finds the cause
/// in the log rather than in the code.
fn refuse_to_start(reason: &'static str) -> bool {
    tracing::info!(
        target: "ducktape::collab",
        reason,
        "no collaboration pump on this node"
    );
    false
}

/// Everything the pump needs. Cloneable: the sweep half and the receipt half
/// run as one task, but the caller builds this before either exists.
pub(crate) struct Pump {
    commands: mpsc::Sender<noded::NodeCommand>,
    status: noded::StatusCell,
    terminals: noded::ServiceLink,
    workspace: PathBuf,
    /// the chain id every op is bound to and every key is scoped by. Taken from
    /// the workspace at boot: a node serves exactly one network.
    network: String,
    /// what the chain still owes, PER MESSAGE and in reported order. See
    /// [`Pump::owe`]. `std::sync::Mutex`: every critical section takes what it
    /// needs and drops the guard before any `.await`.
    owed: std::sync::Mutex<BTreeMap<Message, std::collections::VecDeque<Unsent>>>,
}

/// one message's delivery record, as the module keys it: the channel, the
/// recipient's party handle, and the channel sequence.
type Message = (String, String, u64);

/// one receipt the daemon reported and the chain has not taken.
#[derive(Debug, Clone)]
struct Unsent {
    /// the generation the daemon reported, carried VERBATIM through every
    /// retry. Substituting the current one would let a stale device's receipt
    /// pass the module's fence on the second attempt.
    credential: collab::Credential,
    state: wire::State,
    reason: Option<String>,
}

/// What the daemon has already been told about one binding.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Announced {
    /// the binding credential last sent as a `MsgBind` generation. 0 = never.
    credential: collab::Credential,
    /// the next committed event sequence to read.
    cursor: u64,
}

/// What one eligibility question answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eligible {
    /// The network says a new attempt may be made, and this record is the one
    /// this pump owns.
    Deliver,
    /// The network answered, and the answer is no. Settled; nothing to revisit.
    No,
    /// The question could not be ASKED. Nothing is known, so nothing is done —
    /// and the message stays in front of the cursor for the next sweep.
    Unresolved,
}

/// What the committed record says about a receipt the chain did not take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Stop carrying it, for a reason READ BACK off the chain.
    Retire(&'static str),
    /// The record is behind this transition: `Queued` has to commit before it
    /// can. The daemon's own `Queued` receipt was lost, and this is the only
    /// bridge the delivery diagram offers.
    Bridge,
    /// Nothing was learned, or the transition is simply still pending. Keep it.
    Owe,
}

/// Everything one pump run remembers between sweeps.
#[derive(Debug, Default)]
struct Seen {
    /// which daemon this was learned about. A different one knows none of it
    /// ([`noded::ServiceLink::attach_epoch`]).
    epoch: u64,
    /// the agreed clock value last sent as a `MsgTime`.
    time: u64,
    /// how many receipts the term plane had dropped when this pump last looked
    /// ([`noded::ServiceLink::dropped_receipts`]).
    dropped: u64,
    /// keyed by (channel, participant handle) — the binding's identity.
    bindings: BTreeMap<(String, String), Announced>,
}

impl Pump {
    /// Build the pump for `workspace` on `network`.
    pub(crate) fn new(
        commands: mpsc::Sender<noded::NodeCommand>,
        status: noded::StatusCell,
        terminals: noded::ServiceLink,
        workspace: PathBuf,
        network: String,
    ) -> Self {
        Self {
            commands,
            status,
            terminals,
            workspace,
            network,
            owed: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Take the daemon's receipt lane and run until it closes.
    ///
    /// `wake` is the sweep trigger and it is a CHANNEL, not a timer, so a test
    /// drives a sweep by sending on it and observes the effects on the daemon's
    /// command lane — no sleeping, no polling. Production feeds it from a
    /// ticker ([`spawn`]).
    pub(crate) async fn run(
        self,
        mut receipts: lane::Receiver<wire::Event>,
        mut wake: lane::Receiver<()>,
    ) {
        let mut seen = Seen::default();
        loop {
            // both arms are cancel-safe `recv`s, so the loser of a race loses
            // nothing: its message is still queued for the next turn.
            tokio::select! {
                event = receipts.recv() => match event {
                    Some(event) => self.receipt(event).await,
                    None => return self.closing("receipt_lane_closed"),
                },
                tick = wake.recv() => match tick {
                    Some(()) => self.sweep(&mut seen).await,
                    None => return self.closing("wake_lane_closed"),
                },
            }
        }
    }

    fn closing(&self, reason: &'static str) {
        tracing::info!(
            target: "ducktape::collab",
            reason,
            "collaboration pump stopping"
        );
    }

    // ---- committed state -> the daemon -------------------------------------

    /// One pass over every binding this device holds a key for.
    ///
    /// Gated on an ATTACHED DAEMON. Without one every command would be dropped
    /// by the link's writer, and a cursor advanced past a message whose
    /// `MsgDeliver` went nowhere is a message nothing looks at again until the
    /// operator re-attaches. Skipping the sweep keeps the cursor where it is.
    async fn sweep(&self, seen: &mut Seen) {
        if !self.terminals.attached() {
            return;
        }
        // a DIFFERENT daemon knows none of what was told to the last one, and
        // "is one attached" cannot tell the two apart — a daemon that dies and
        // redials with the same bindings looks identical to one that never left,
        // and its epoch is what says otherwise. Everything cached is forgotten:
        // the binds, the clock and the cursors all get re-sent, and the
        // daemon's own dedup journal absorbs anything it already had.
        let epoch = self.terminals.attach_epoch();
        if epoch != seen.epoch {
            tracing::info!(
                target: "ducktape::collab",
                epoch,
                reason = "daemon_reattached",
                "re-announcing every binding to a new agent daemon"
            );
            *seen = Seen {
                epoch,
                ..Default::default()
            };
        }
        // a receipt whose submission failed is retried here, before any new
        // delivery: the network learning what already happened comes first.
        self.resubmit().await;
        // a receipt the term plane could not hand over is GONE, and re-reading
        // the chain cannot recover it: the module records what was submitted,
        // and that receipt is precisely the one that never was. The daemon's
        // delivery journal is the only remaining witness, so it is asked to say
        // everything it holds again. Its own report is idempotent here — a
        // transition already owed folds ([`push_once`]) and one already
        // committed retires on the read ([`Pump::standing`]).
        let dropped = self.terminals.dropped_receipts();
        let lost_a_receipt = dropped != seen.dropped;
        seen.dropped = dropped;
        let now = self.status.current().consensus_time;
        // the daemon owns no clock; this is the only thing that advances the
        // one it judges expiry against. Sent before anything is delivered, so a
        // message handed over in this sweep is judged against this value and not
        // the older one frozen onto its frame at admission.
        if now > seen.time {
            seen.time = now;
            self.terminals
                .send(wire::Command::MsgTime { network_now: now })
                .await;
        }
        let attachments = match crate::collab_keys::attached(&self.workspace, &self.network) {
            Ok(attachments) => attachments,
            Err(error) => return self.refuse("attachments_unreadable", &error),
        };
        for attachment in attachments {
            let key = (
                attachment.conversation.clone(),
                attachment.participant.clone(),
            );
            if lost_a_receipt {
                self.terminals
                    .send(wire::Command::MsgReplay {
                        conversation: attachment.conversation.clone(),
                        participant: attachment.participant.clone(),
                    })
                    .await;
            }
            let mut announced = seen.bindings.get(&key).copied().unwrap_or_default();
            self.pump_one(&attachment, &mut announced).await;
            seen.bindings.insert(key, announced);
        }
    }

    /// Bring one binding up to date: its generation, and every delivery
    /// requested for it since this pump last looked.
    async fn pump_one(&self, attachment: &Attached, announced: &mut Announced) {
        let binding = crate::collab_keys::BindingRef {
            network: &self.network,
            conversation: &attachment.conversation,
            participant: &attachment.participant,
        };
        let key = match crate::collab_keys::load(&self.workspace, binding) {
            Ok(Some(key)) => key,
            // listed but keyless: the operator removed the key file. Nothing to
            // authenticate with, so there is nothing to say about it.
            Ok(None) => return self.skip(attachment, "no_service_key"),
            Err(error) => return self.refuse("service_key_unreadable", &error),
        };
        let Some(collab::CollaborationReply::Binding(binding)) = self
            .read(
                attachment,
                &key,
                collab::ProtectedRead::Binding {
                    channel_id: attachment.conversation.clone(),
                },
            )
            .await
        else {
            return;
        };
        // no live binding: the owner never signed the `Bind`, or an `Unbind`
        // spent it. Forget the generation so a later re-bind is sent as a fresh
        // one rather than looking unchanged.
        let credential = binding
            .filter(|binding| !binding.detached)
            .map_or(0, |binding| binding.credential);
        if credential == 0 {
            announced.credential = 0;
            return self.skip(attachment, "unbound");
        }
        if announced.credential != credential {
            announced.credential = credential;
            // a new attachment has seen none of the channel: read from the
            // beginning, where everything may still be waiting for this
            // participant.
            announced.cursor = 0;
            self.terminals
                .send(wire::Command::MsgBind(wire::Bind {
                    conversation: attachment.conversation.clone(),
                    participant: attachment.participant.clone(),
                    generation: credential,
                    device: attachment.device.clone(),
                }))
                .await;
        }
        self.drain_events(attachment, &key, announced).await;
    }

    /// Read forward from the cursor, delivering what is requested for this
    /// participant.
    async fn drain_events(
        &self,
        attachment: &Attached,
        key: &commonware_cryptography::ed25519::PrivateKey,
        announced: &mut Announced,
    ) {
        for _ in 0..PAGES_PER_SWEEP {
            let Some(collab::CollaborationReply::Events(page)) = self
                .read(
                    attachment,
                    key,
                    collab::ProtectedRead::Events {
                        channel_id: attachment.conversation.clone(),
                        from_seq: announced.cursor,
                        limit: PAGE,
                    },
                )
                .await
            else {
                return;
            };
            let collab::EventPage {
                events,
                deliveries,
                next_seq,
            } = page;
            // a short page reached the channel head.
            let exhausted = (events.len() as u64) < PAGE;
            match self
                .offer_page(attachment, key, announced.credential, &events, &deliveries)
                .await
            {
                // this page handed something over, or could not establish what
                // to do with something on it. STOP, with the cursor ON that
                // message: the next sweep reads the page again and either walks
                // past the receipt that has since committed, or retries the read
                // that failed.
                Some(hold) => {
                    announced.cursor = hold;
                    return;
                }
                None => announced.cursor = next_seq,
            }
            if exhausted {
                return;
            }
        }
    }

    /// Hand every message on one page that is requested for this participant
    /// to the daemon. Answers the FIRST sequence the cursor must not advance
    /// past, or `None` when every message on the page was resolved and none
    /// delivered.
    ///
    /// Two things hold the cursor, and both must:
    ///
    /// * a message handed over — it stays `Stored` until the daemon's `Queued`
    ///   receipt commits a block later, and a cursor past it is a message
    ///   nothing looks at again if the link died between the two;
    /// * a message whose eligibility could not be ESTABLISHED — a dropped actor
    ///   lane, a refused read, a reply this build cannot decode. Walking past
    ///   one silently drops a queued message on a transient failure, which is
    ///   the same loss with none of the evidence.
    ///
    /// Later messages on the page are still offered either way: one unresolved
    /// read must not stall the mail behind it.
    async fn offer_page(
        &self,
        attachment: &Attached,
        key: &commonware_cryptography::ed25519::PrivateKey,
        credential: collab::Credential,
        events: &[collab::ChannelEvent],
        deliveries: &[collab::Delivery],
    ) -> Option<u64> {
        let mut hold: Option<u64> = None;
        let mut keep = |seq: u64| hold = Some(hold.map_or(seq, |first: u64| first.min(seq)));
        let Ok(participant) = collab::parse_party_handle(&attachment.participant) else {
            self.refuse(
                "participant_unparseable",
                "an attachment names a participant that is not a party handle",
            );
            return None;
        };
        for seq in requested_for(&participant, events) {
            let Some(delivery) = deliveries
                .iter()
                .find(|delivery| delivery.seq == seq && delivery.recipient == participant)
            else {
                // the page joins every requested record it names; one missing
                // is a read this build cannot make sense of, not an absence.
                self.unresolved(attachment, seq, "record_missing");
                keep(seq);
                continue;
            };
            // the bound, applied where there is still a choice: a receipt is
            // already spent by the time it arrives, but a delivery is not. The
            // message stays `Stored` in front of the cursor and goes out when
            // the owed chain drains.
            if self.overloaded() {
                keep(seq);
                continue;
            }
            match self.eligible(attachment, key, seq).await {
                Eligible::Deliver => keep(seq),
                Eligible::No => continue,
                Eligible::Unresolved => {
                    keep(seq);
                    continue;
                }
            }
            // the body is chat's, read AFTER eligibility so an expired or
            // settled record costs no chat read at all.
            let Some(message) = self.chat_message(&delivery.message_id).await else {
                self.unresolved(attachment, seq, "chat_read_failed");
                continue;
            };
            self.terminals
                .send(wire::Command::MsgDeliver(Box::new(deliver(
                    delivery, &message, credential,
                ))))
                .await;
            tracing::debug!(
                target: "ducktape::collab",
                conversation = %attachment.conversation,
                seq,
                "offered a message to the daemon"
            );
        }
        hold
    }

    /// May a NEW delivery attempt be made for this message, right now?
    ///
    /// Asked immediately before the handover and answered against the block's
    /// agreed time. Only a `Stored` record is work: every other state either
    /// belongs to a durable record this pump does not own, or is settled.
    ///
    /// Three answers and not two. "The network says no" and "I could not ask"
    /// are different facts: the first is settled and the message is finished
    /// with, the second is a transient failure whose message must be looked at
    /// again. Collapsing them into one `false` is how a queued message gets
    /// walked past and never delivered.
    async fn eligible(
        &self,
        attachment: &Attached,
        key: &commonware_cryptography::ed25519::PrivateKey,
        seq: u64,
    ) -> Eligible {
        let answer = self
            .read(
                attachment,
                key,
                collab::ProtectedRead::DeliveryEligibility {
                    channel_id: attachment.conversation.clone(),
                    seq,
                },
            )
            .await;
        let reason = match answer {
            Some(collab::CollaborationReply::Eligibility(verdict)) => {
                let Err(reason) = admits_delivery(&verdict) else {
                    return Eligible::Deliver;
                };
                reason
            }
            // NOT permission, and not a refusal either: the module was never
            // heard from, or answered something this build cannot read as an
            // eligibility. `read` has already named which.
            None => return self.unresolved(attachment, seq, "read_failed"),
            Some(_) => return self.unresolved(attachment, seq, "unexpected_reply"),
        };
        tracing::debug!(
            target: "ducktape::collab",
            conversation = %attachment.conversation,
            seq,
            reason,
            "not offering a message to the daemon"
        );
        Eligible::No
    }

    /// the eligibility question could not be asked. Latched, because whatever
    /// stopped it stops every message behind it in the same sweep.
    fn unresolved(&self, attachment: &Attached, seq: u64, reason: &'static str) -> Eligible {
        if let Some(occurrences) = PUMP_WARN.hit(reason) {
            tracing::warn!(
                target: "ducktape::collab",
                conversation = %attachment.conversation,
                seq,
                reason,
                occurrences,
                "could not establish whether a message may be delivered; holding it"
            );
        }
        Eligible::Unresolved
    }

    // ---- the daemon's receipts -> committed state ---------------------------

    /// THE dispatch for everything the daemon reports about collaboration. One
    /// arm per variant, each a single delegation.
    async fn receipt(&self, event: wire::Event) {
        match event {
            wire::Event::MsgBound {
                conversation,
                participant,
                generation,
                capabilities,
            } => self.bound(&conversation, &participant, generation, capabilities),
            wire::Event::MsgBindRefused {
                conversation,
                participant,
                generation,
                reason,
            } => self.bind_refused(&conversation, &participant, generation, reason),
            wire::Event::MsgDelivery {
                conversation,
                participant,
                seq,
                binding_generation,
                state,
                reason,
                ..
            } => {
                self.acknowledge(
                    &conversation,
                    &participant,
                    seq,
                    binding_generation,
                    state,
                    reason,
                )
                .await;
            }
            // the term plane routes only the three above to this lane, so these
            // cannot arrive — named rather than wildcarded so a receipt added to
            // either plane fails the build here instead of being swallowed.
            misrouted @ (wire::Event::TermCreated { .. }
            | wire::Event::TermRefused { .. }
            | wire::Event::TermOutput { .. }
            | wire::Event::TermEnded { .. }) => self.misrouted(&misrouted),
        }
    }

    /// the binding is live on the daemon. A lifecycle fact, once per bind.
    fn bound(
        &self,
        conversation: &str,
        participant: &str,
        generation: collab::Credential,
        capabilities: wire::Capabilities,
    ) {
        tracing::info!(
            target: "ducktape::collab",
            conversation = %conversation,
            participant = %participant,
            generation,
            accepts_while_busy = capabilities.accepts_while_busy,
            wakes_idle = capabilities.wakes_idle,
            reports_acceptance = capabilities.reports_acceptance,
            steers_active_turn = capabilities.steers_active_turn,
            "a collaboration binding is live on the agent daemon"
        );
    }

    /// the daemon refused the bind. Nothing is retried here: every refusal names
    /// a state only the operator or a fresh committed `Bind` can change.
    fn bind_refused(
        &self,
        conversation: &str,
        participant: &str,
        generation: collab::Credential,
        reason: wire::BindRefusal,
    ) {
        tracing::warn!(
            target: "ducktape::collab",
            conversation = %conversation,
            participant = %participant,
            generation,
            reason = refusal_token(reason),
            "the agent daemon refused a collaboration binding"
        );
    }

    /// Commit what the daemon observed, as the binding itself.
    ///
    /// `binding_generation` becomes `binding_credential` verbatim: the daemon
    /// echoes the generation the delivery was aimed at, and the module refuses
    /// one that is not current. That is the fence — a returning stale device
    /// cannot overwrite the state of the attachment that replaced it, and this
    /// must not "helpfully" substitute the credential it just read.
    async fn acknowledge(
        &self,
        conversation: &str,
        participant: &str,
        seq: u64,
        binding_generation: collab::Credential,
        state: wire::State,
        reason: Option<String>,
    ) {
        let message = (conversation.to_string(), participant.to_string(), seq);
        let unsent = Unsent {
            credential: binding_generation,
            state,
            reason,
        };
        // BEHIND whatever this message already owes. The diagram is a chain —
        // `Stored -> Queued -> AdapterAccepted` — so submitting this now, while
        // an earlier transition of the SAME message is still waiting, is the
        // out-of-order submission the module refuses. Another message's queue
        // is unrelated and is not waited on.
        if self.owe_behind(&message, &unsent) {
            return;
        }
        self.commit_receipt(&message, unsent).await;
    }

    /// Queue `unsent` behind this message's existing debt, if it has any.
    /// Answers whether it was queued.
    fn owe_behind(&self, message: &Message, unsent: &Unsent) -> bool {
        let mut owed = self.owed.lock().expect("collab owed lock poisoned");
        let Some(chain) = owed.get_mut(message) else {
            return false;
        };
        push_once(chain, unsent);
        true
    }

    /// Submit one receipt, and OWE it if the submission did not land.
    ///
    /// A dropped acknowledgement is not self-healing. Once `Queued` commits, the
    /// eligibility read answers `already_queued` forever, so the pump never
    /// re-offers the message and the daemon never re-reports it: a failed
    /// `AdapterAccepted` submission would leave the network reading `Queued` for
    /// a message a provider took, with no operator action short of re-binding
    /// able to correct it.
    async fn commit_receipt(&self, message: &Message, unsent: Unsent) {
        let (conversation, participant, seq) = message;
        let binding = crate::collab_keys::BindingRef {
            network: &self.network,
            conversation,
            participant,
        };
        let key = match crate::collab_keys::load(&self.workspace, binding) {
            Ok(Some(key)) => key,
            // ABSENT is permanent: the operator removed the key, and nothing
            // here can ever sign for that binding again.
            Ok(None) => {
                return self.refuse(
                    "receipt_without_key",
                    "a receipt named a binding this device holds no service key for",
                );
            }
            // UNREADABLE is not. A permissions problem or a half-written file is
            // a local fault that clears, and a fact must not be thrown away
            // because this process could not open a file for a moment. Owed with
            // nothing to verify against, because verifying also needs the key.
            Err(error) => return self.owe(message, unsent, None, &error).await,
        };
        let Ok(recipient) = collab::parse_party_handle(participant) else {
            return self.refuse(
                "participant_unparseable",
                "a receipt named a participant that is not a party handle",
            );
        };
        let op = collab::CollaborationMsg::Acknowledge {
            channel_id: conversation.clone(),
            seq: *seq,
            recipient,
            binding_credential: unsent.credential,
            state: delivery_state(unsent.state),
            reason: unsent.reason.clone(),
        };
        match self.submit(&key, op).await {
            Ok(height) => tracing::debug!(
                target: "ducktape::collab",
                conversation = %conversation,
                seq,
                height,
                "acknowledged a delivery on-chain"
            ),
            Err(error) => self.owe(message, unsent, Some(&key), &error).await,
        }
    }

    /// Keep a receipt the chain did not take — unless the chain has ALREADY
    /// been shown to hold the fact, or to have moved somewhere this transition
    /// can never reach.
    ///
    /// There is no attempt counter, deliberately. A count cannot tell a busy
    /// actor from a permanent refusal, so counting means eventually throwing
    /// away a fact that was merely unlucky. The committed record can tell:
    /// [`Pump::standing`] reads it and retires the receipt only on a VERIFIED
    /// outcome. Everything else is owed, for as long as it takes — and when the
    /// record is merely BEHIND, the missing `Queued` goes in front of it so the
    /// pair submits in the order the diagram admits.
    ///
    /// `verify` is the binding's key when one could be loaded. Without it the
    /// receipt is simply owed: an unreadable key is exactly the transient fault
    /// that must not cost a fact, and it is also the thing a verification would
    /// have needed.
    ///
    /// This never turns a receipt away. The daemon has already SPENT the fact by
    /// reporting it — refusing here would drop it with nobody to re-report it,
    /// which is the eviction this whole design refuses. The table is instead
    /// bounded upstream, where the pump still has a choice: it stops handing new
    /// messages to the daemon while it is this far behind ([`Pump::overloaded`]),
    /// and a repeated report of a transition already owed is folded rather than
    /// appended ([`push_once`]), so a daemon replaying its journal cannot grow
    /// one message's chain past the few transitions the diagram allows.
    async fn owe(
        &self,
        message: &Message,
        unsent: Unsent,
        verify: Option<&commonware_cryptography::ed25519::PrivateKey>,
        error: &str,
    ) {
        let standing = match verify {
            Some(key) => self.standing(message, unsent.state, key).await,
            None => Standing::Owe,
        };
        let bridge = match standing {
            Standing::Retire(reason) => return self.refuse(reason, error),
            Standing::Bridge => Some(Unsent {
                // the daemon's generation, not the current one: the bridge is
                // part of the same report and passes the same fence.
                credential: unsent.credential,
                state: wire::State::Queued,
                // nothing to say about it. The daemon never told us why it
                // queued this, and inventing a token would put a sentence on
                // chain that no service ever said.
                reason: None,
            }),
            Standing::Owe => None,
        };
        let owing = {
            let mut owed = self.owed.lock().expect("collab owed lock poisoned");
            let chain = owed.entry(message.clone()).or_default();
            if let Some(bridge) = bridge {
                push_once(chain, &bridge);
            }
            push_once(chain, &unsent);
            owed.len()
        };
        if owing >= MAX_OWING_MESSAGES {
            self.refuse(
                "owed_receipts_high",
                "no new message goes to the daemon until the chain catches up",
            );
        }
        self.refuse("acknowledge_retrying", error);
    }

    /// Is the chain so far behind that no NEW message should be handed over?
    ///
    /// This is where the bound is applied, and it is the only place it CAN be: a
    /// receipt has already been spent by the time it reaches [`Pump::owe`], but
    /// a delivery has not been made yet. Holding one back costs a sweep; the
    /// message stays `Stored`, in front of the cursor, and goes out when the
    /// backlog drains. Nothing is lost and nothing is dropped.
    fn overloaded(&self) -> bool {
        self.owed.lock().expect("collab owed lock poisoned").len() >= MAX_OWING_MESSAGES
    }

    /// Where does the committed record leave this transition?
    ///
    /// [`Standing::Retire`] only on a fact READ BACK off the chain:
    ///
    /// * the committed state IS the one being reported — it landed after all
    ///   (a lost reply is indistinguishable from a lost submission from here);
    /// * there is no record — the message was pruned, and nothing will accept a
    ///   receipt for it;
    /// * the record can never reach the reported state, by the diagram — every
    ///   transition out of a terminal state, and the few non-terminal pairs the
    ///   diagram simply does not join. Carrying one of those forever is a leak
    ///   with no outcome at the end of it.
    ///
    /// [`Standing::Owe`] on every read failure. An unverified receipt is never
    /// abandoned.
    async fn standing(
        &self,
        message: &Message,
        state: wire::State,
        key: &commonware_cryptography::ed25519::PrivateKey,
    ) -> Standing {
        let (conversation, participant, seq) = message;
        // the device label is not part of a READ — only the participant acting
        // and the conversation its key is scoped to are.
        let attachment = Attached {
            network: self.network.clone(),
            conversation: conversation.clone(),
            participant: participant.clone(),
            device: String::new(),
        };
        let answer = self
            .read(
                &attachment,
                key,
                collab::ProtectedRead::Delivery {
                    channel_id: conversation.clone(),
                    seq: *seq,
                },
            )
            .await;
        // an unreadable answer, or one this build cannot interpret: nothing has
        // been verified, so nothing is given up.
        let Some(collab::CollaborationReply::Delivery(receipt)) = answer else {
            return Standing::Owe;
        };
        let Some(receipt) = receipt else {
            return Standing::Retire("receipt_gone");
        };
        let reported = delivery_state(state);
        if receipt.state == reported {
            return Standing::Retire("acknowledge_already_landed");
        }
        // the diagram, asked directly. `Queued` is the ONLY state anything
        // bridges through — a record still `Stored` because the daemon's queue
        // receipt was lost cannot take the acceptance that followed it.
        let directly = receipt.state.may_advance_to(reported);
        let through_queued = receipt.state.may_advance_to(collab::DeliveryState::Queued)
            && collab::DeliveryState::Queued.may_advance_to(reported);
        match (directly, through_queued) {
            (true, _) => Standing::Owe,
            (false, true) => Standing::Bridge,
            (false, false) => Standing::Retire("acknowledge_unreachable"),
        }
    }

    /// Retry what the chain is owed, per message and in reported order.
    ///
    /// A message stops at its FIRST failure and keeps the rest of its chain
    /// behind it — that is what makes this a recovery rather than a second way
    /// to lose the record, because the diagram refuses a transition taken out of
    /// order. Messages do not wait on each other.
    async fn resubmit(&self) {
        let debts: Vec<(Message, std::collections::VecDeque<Unsent>)> = {
            let mut owed = self.owed.lock().expect("collab owed lock poisoned");
            std::mem::take(&mut *owed).into_iter().collect()
        };
        for (message, chain) in debts {
            for (offset, unsent) in chain.iter().enumerate() {
                self.commit_receipt(&message, unsent.clone()).await;
                let still_owing = self
                    .owed
                    .lock()
                    .expect("collab owed lock poisoned")
                    .contains_key(&message);
                if still_owing {
                    // it failed again and re-owed itself. Everything after it
                    // goes back behind it, unattempted.
                    let mut owed = self.owed.lock().expect("collab owed lock poisoned");
                    let queue = owed.entry(message.clone()).or_default();
                    for later in chain.iter().skip(offset + 1) {
                        queue.push_back(later.clone());
                    }
                    break;
                }
            }
        }
    }

    fn misrouted(&self, event: &wire::Event) {
        let kind = match event {
            wire::Event::TermCreated { .. } => "term_created",
            wire::Event::TermRefused { .. } => "term_refused",
            wire::Event::TermOutput { .. } => "term_output",
            wire::Event::TermEnded { .. } => "term_ended",
            wire::Event::MsgBound { .. }
            | wire::Event::MsgBindRefused { .. }
            | wire::Event::MsgDelivery { .. } => "collab",
        };
        tracing::warn!(
            target: "ducktape::collab",
            reason = "misrouted_event",
            event = kind,
            "a terminal event reached the collaboration pump"
        );
    }

    // ---- the two lanes to the node actor ------------------------------------

    /// One authenticated read, as the binding's scoped service key.
    ///
    /// `None` on every failure — a closed actor lane, a module refusal, an
    /// undecodable reply — each with its own reason token. A caller treats
    /// `None` as "learned nothing", never as a permissive default.
    async fn read(
        &self,
        attachment: &Attached,
        key: &commonware_cryptography::ed25519::PrivateKey,
        read: collab::ProtectedRead,
    ) -> Option<collab::CollaborationReply> {
        let Ok(participant) = collab::parse_party_handle(&attachment.participant) else {
            self.refuse(
                "participant_unparseable",
                "an attachment names a participant that is not a party handle",
            );
            return None;
        };
        let query = collab::CollaborationQuery::Read {
            participant,
            // the key is scoped to ONE channel and the store is hashed, so the
            // module cannot find the binding that holds it by scanning. The
            // caller names the one it holds.
            via: Some(attachment.conversation.clone()),
            read,
        };
        let (reply, answer) = oneshot::channel();
        let sent = self
            .commands
            .clone()
            .send(noded::NodeCommand::QueryAs {
                target: COLLABORATION.to_string(),
                req: collab::encode_query(&query),
                reader: commonware_cryptography::Signer::public_key(key)
                    .as_ref()
                    .to_vec(),
                reply,
            })
            .await;
        if sent.is_err() {
            self.refuse("node_actor_gone", "the node actor is not accepting reads");
            return None;
        }
        let bytes = match answer.await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                self.refuse("read_refused", &error.message);
                return None;
            }
            Err(_) => {
                self.refuse("node_actor_gone", "the node actor dropped a read");
                return None;
            }
        };
        match collab::decode_reply(&bytes) {
            Ok(collab::CollaborationReply::Denied(denial)) => {
                self.refuse("read_denied", denial_token(denial));
                None
            }
            Ok(reply) => Some(reply),
            Err(error) => {
                self.refuse("reply_undecodable", &error);
                None
            }
        }
    }

    /// One chat message by id, over the node's public query lane.
    ///
    /// `None` on every failure, each with its own reason token, and also for a
    /// message chat no longer holds — a delivery record outlives nothing, but
    /// a read this build cannot make sense of is not a body to hand over.
    async fn chat_message(&self, message_id: &str) -> Option<chat::MessageView> {
        let (reply, answer) = oneshot::channel();
        let sent = self
            .commands
            .clone()
            .send(noded::NodeCommand::Query {
                target: chat::DEFAULT_CHAT_TARGET.to_string(),
                req: chat::encode_query(&chat::ChatQuery::Message {
                    message_id: message_id.to_string(),
                }),
                reply,
            })
            .await;
        if sent.is_err() {
            self.refuse("node_actor_gone", "the node actor is not accepting reads");
            return None;
        }
        let bytes = match answer.await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                self.refuse("chat_read_refused", &error.message);
                return None;
            }
            Err(_) => {
                self.refuse("node_actor_gone", "the node actor dropped a read");
                return None;
            }
        };
        match chat::decode_reply(&bytes) {
            Ok(chat::ChatReply::Message(Some(message))) => Some(message),
            Ok(chat::ChatReply::Message(None)) => {
                self.refuse("chat_message_gone", message_id);
                None
            }
            Ok(_) => {
                self.refuse("chat_reply_unexpected", message_id);
                None
            }
            Err(error) => {
                self.refuse("chat_reply_undecodable", &error);
                None
            }
        }
    }

    /// One collaboration op, signed by the binding's scoped service key.
    ///
    /// The op travels inside a [`collab::Request`] naming the network, and that
    /// name is inside the SIGNED payload: a submit frame carries no chain id, so
    /// without it these bytes would verify on any network the same key may
    /// submit to.
    async fn submit(
        &self,
        key: &commonware_cryptography::ed25519::PrivateKey,
        op: collab::CollaborationMsg,
    ) -> Result<u64, String> {
        let request = collab::Request::new(self.network.clone(), op);
        let frame =
            crate::userkey_cli::user_frame(key, COLLABORATION, collab::encode_msg(&request));
        let (reply, answer) = oneshot::channel();
        self.commands
            .clone()
            .send(noded::NodeCommand::SubmitFrame { frame, reply })
            .await
            .map_err(|_| "the node actor is not accepting submissions".to_string())?;
        match answer.await {
            Ok(Ok(block)) => Ok(block.height),
            Ok(Err(refused)) => Err(refused.message),
            Err(_) => Err("the node actor dropped a submission".to_string()),
        }
    }

    // ---- the two ways a sweep says nothing happened -------------------------

    /// something this pump depends on is unusable. Latched: a gone actor or an
    /// unreadable key repeats every sweep.
    fn refuse(&self, reason: &'static str, detail: &str) {
        if let Some(occurrences) = PUMP_WARN.hit(reason) {
            tracing::warn!(
                target: "ducktape::collab",
                reason,
                occurrences,
                "collaboration pump: {detail}"
            );
        }
    }

    /// an ordinary state that means there is nothing to do for one binding.
    fn skip(&self, attachment: &Attached, reason: &'static str) {
        tracing::debug!(
            target: "ducktape::collab",
            conversation = %attachment.conversation,
            participant = %attachment.participant,
            reason,
            "nothing to pump for this binding"
        );
    }
}

/// per-sweep `ducktape::collab` warns that repeat for as long as their cause
/// stands: an unreadable key, a gone actor, a refused read. First occurrence,
/// then every 100th, carrying `occurrences`.
static PUMP_WARN: noded::log::Latch = noded::log::Latch::new(100);

// ---- decisions, made without touching anything -----------------------------

/// Append one owed transition, unless this message already owes exactly it.
///
/// A daemon that restarts replays its durable journal, so the SAME
/// `(credential, state)` can be reported many times over. Appending each would
/// grow one message's chain without bound and re-submit a transition the module
/// would refuse as already made. Folding is safe because the pair is the whole
/// content of the op: two identical entries produce two identical submissions.
///
/// The `reason` is deliberately not compared. It is a token about the same
/// transition, so a second report of it is the same fact told slightly
/// differently, not a new one.
fn push_once(chain: &mut std::collections::VecDeque<Unsent>, unsent: &Unsent) {
    let already = chain
        .iter()
        .any(|owed| owed.credential == unsent.credential && owed.state == unsent.state);
    if already {
        return;
    }
    chain.push_back(unsent.clone());
}

/// The channel sequences on this page whose delivery was requested FOR
/// `participant` — the sequence of the MESSAGE, which is what the record is
/// keyed on, not of the request event.
///
/// A `DeliveryAdvanced` on the page is this pump's own acknowledgement coming
/// back, or an expiry somebody ran; neither is work. A `BindingChanged` is
/// read from `Binding` on the next sweep, which is the authority for it.
fn requested_for(participant: &collab::Party, events: &[collab::ChannelEvent]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match &event.body {
            collab::EventBody::DeliveryRequested {
                message_seq,
                recipient,
                ..
            } if recipient == participant => Some(*message_seq),
            collab::EventBody::DeliveryRequested { .. }
            | collab::EventBody::DeliveryAdvanced { .. }
            | collab::EventBody::BindingChanged { .. } => None,
        })
        .collect()
}

/// Whether this verdict admits a NEW delivery, or the token naming why not.
///
/// `Eligible` alone is not enough: it is returned for every non-terminal state,
/// and only `Stored` is work this pump owns. `Queued` and `Held` are records
/// something else holds durably, and re-offering either would duplicate an
/// instruction a model has already been given.
fn admits_delivery(verdict: &collab::DeliveryEligibility) -> Result<(), &'static str> {
    let collab::DeliveryEligibility::Eligible { state, .. } = verdict else {
        return Err(match verdict {
            collab::DeliveryEligibility::Eligible { .. } => unreachable!("matched above"),
            collab::DeliveryEligibility::Expired { .. } => "expired",
            collab::DeliveryEligibility::Settled { .. } => "settled",
            collab::DeliveryEligibility::NotReplayable => "not_replayable",
            collab::DeliveryEligibility::Unbound => "unbound",
            collab::DeliveryEligibility::Unknown => "unknown",
        });
    };
    match state {
        collab::DeliveryState::Stored => Ok(()),
        collab::DeliveryState::Queued => Err("already_queued"),
        collab::DeliveryState::Held => Err("held"),
        collab::DeliveryState::DeliveryUnknown => Err("not_replayable"),
        collab::DeliveryState::AdapterAccepted
        | collab::DeliveryState::Refused
        | collab::DeliveryState::Expired => Err("settled"),
    }
}

/// The committed delivery record and its chat message as the frame the daemon
/// places into a session.
///
/// A straight projection with nothing added: every field is one module's or
/// the other's, and `urgent` is false because no committed field says
/// otherwise — steering an active turn is a sender's explicit request and
/// neither module carries one, so inventing it here would let the pump
/// interrupt a model on its own authority.
///
/// `credential` is the RECIPIENT's current binding credential, read this sweep.
/// It is the fence: a daemon holding a newer generation drops this delivery
/// rather than handing a message aimed at a replaced attachment to the device
/// that replaced it.
fn deliver(
    delivery: &collab::Delivery,
    message: &chat::MessageView,
    credential: collab::Credential,
) -> wire::Deliver {
    wire::Deliver {
        conversation: delivery.channel_id.clone(),
        participant: handle(&delivery.recipient),
        seq: delivery.seq,
        binding_generation: credential,
        message_id: delivery.message_id.clone(),
        sender: handle(&delivery.sender),
        kind: kind(delivery.kind),
        task: delivery.task.as_ref().map(|task| wire::TaskRef {
            id: task.id.clone(),
            expected_attempt: task.expected_attempt,
        }),
        reply_to: message.head.thread,
        body: plain_text(&message.head.blocks),
        references: delivery.references.iter().map(reference).collect(),
        expires_at: delivery.expires_at,
        network_now: delivery.requested_at,
        urgent: false,
    }
}

/// a party as the daemon's wire spells it. Every party a delivery record names
/// is an account or a key — the module admits nothing else — so the handle
/// always exists; the empty string is the fail-closed spelling of one that
/// somehow does not, and no daemon binding matches it.
fn handle(party: &collab::Party) -> String {
    collab::party_handle(party).unwrap_or_default()
}

/// a chat body as the plain text a provider session sees: spans joined by
/// spaces, code kept verbatim, a divider dropped.
fn plain_text(blocks: &[chat::Block]) -> String {
    fn spans(out: &mut String, spans: &[chat::Span]) {
        for span in spans {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(&span.text);
        }
    }
    let mut out = String::new();
    for block in blocks {
        if !out.is_empty() {
            out.push('\n');
        }
        match block {
            chat::Block::Paragraph(s) | chat::Block::Quote(s) => spans(&mut out, s),
            chat::Block::Code { text, .. } => out.push_str(text),
            chat::Block::Divider => {}
        }
    }
    out
}

fn kind(kind: collab::MessageKind) -> wire::Kind {
    match kind {
        collab::MessageKind::Notice => wire::Kind::Notice,
        collab::MessageKind::Question => wire::Kind::Question,
        collab::MessageKind::TaskRequest => wire::Kind::TaskRequest,
        collab::MessageKind::TaskUpdate => wire::Kind::TaskUpdate,
        collab::MessageKind::Result => wire::Kind::Result,
    }
}

fn reference(reference: &collab::Reference) -> wire::Reference {
    match reference {
        collab::Reference::Commit { repo, commit } => wire::Reference::Commit {
            repo: repo.clone(),
            commit: commit.clone(),
        },
        collab::Reference::Blob { hash } => wire::Reference::Blob { hash: hash.clone() },
        collab::Reference::Duck { url } => wire::Reference::Duck { url: url.clone() },
    }
}

/// What the daemon observed, as the module's own state.
///
/// `DeliveryState::Stored` has no arm because it is the NETWORK's: it means the
/// module admitted the record and no service has queued it. A daemon reporting
/// it would be reporting somebody else's fact.
fn delivery_state(state: wire::State) -> collab::DeliveryState {
    match state {
        wire::State::Queued => collab::DeliveryState::Queued,
        wire::State::AdapterAccepted => collab::DeliveryState::AdapterAccepted,
        wire::State::Held => collab::DeliveryState::Held,
        wire::State::Refused => collab::DeliveryState::Refused,
        wire::State::Expired => collab::DeliveryState::Expired,
        wire::State::DeliveryUnknown => collab::DeliveryState::DeliveryUnknown,
    }
}

fn refusal_token(reason: wire::BindRefusal) -> &'static str {
    match reason {
        wire::BindRefusal::StaleGeneration => "stale_generation",
        wire::BindRefusal::UnknownDevice => "unknown_device",
        wire::BindRefusal::SessionUnreachable => "session_unreachable",
    }
}

fn denial_token(denial: collab::DenyReason) -> &'static str {
    match denial {
        collab::DenyReason::Unauthenticated => "unauthenticated",
        collab::DenyReason::NotReader => "not_reader",
        collab::DenyReason::NotPermitted => "not_permitted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use collab::{DeliveryEligibility as Verdict, DeliveryState as State};

    /// the channel every fixture in here is scoped to.
    const CONVERSATION: &str = "standup";

    fn eligible(state: State) -> Verdict {
        Verdict::Eligible {
            state,
            expires_at: 100,
            asked_at: 10,
        }
    }

    /// THE rule this whole read exists for. A receipt is a LOG: an
    /// `adapter_accepted` in it is a true statement about something that
    /// happened, and reading it as "so it may be delivered" is how a message
    /// gets handed to a model twice.
    #[test]
    fn a_historical_acceptance_is_not_permission_to_deliver_again() {
        assert_eq!(
            admits_delivery(&Verdict::Settled {
                state: State::AdapterAccepted
            }),
            Err("settled"),
            "an accepted record is history, not work"
        );
        assert_eq!(
            admits_delivery(&eligible(State::Stored)),
            Ok(()),
            "an admitted record no service has queued is the one thing to deliver"
        );
    }

    /// The deadline decides, and it is the network's. Every verdict that is not
    /// an eligible `Stored` names why it is not, and none of them is a maybe.
    #[test]
    fn every_verdict_but_a_stored_one_refuses_a_new_delivery() {
        let cases = [
            (
                Verdict::Expired {
                    expires_at: 10,
                    asked_at: 10,
                },
                Err("expired"),
            ),
            (Verdict::NotReplayable, Err("not_replayable")),
            (Verdict::Unbound, Err("unbound")),
            (Verdict::Unknown, Err("unknown")),
            // non-terminal, but not this pump's to move: a durable record
            // somewhere else already owns each of them.
            (eligible(State::Queued), Err("already_queued")),
            (eligible(State::Held), Err("held")),
            // the spec is explicit: never replayed automatically.
            (eligible(State::DeliveryUnknown), Err("not_replayable")),
            (eligible(State::Stored), Ok(())),
        ];
        for (verdict, expected) in cases {
            assert_eq!(
                admits_delivery(&verdict),
                expected,
                "{verdict:?} must answer {expected:?}"
            );
        }
    }

    /// A channel's event stream carries everything that ever happened in it.
    /// Only mail addressed to THIS participant is work — a message to somebody
    /// else on the same channel is not this device's to deliver — and the
    /// sequence that identifies it is the MESSAGE's, not the request event's.
    #[test]
    fn only_mail_addressed_to_this_participant_is_work() {
        let (alice, bob, carol) = (party(1), party(2), party(3));
        let events = vec![
            event(
                3,
                collab::EventBody::DeliveryRequested {
                    message_seq: 1,
                    sender: alice.clone(),
                    recipient: bob.clone(),
                    kind: collab::MessageKind::Notice,
                },
            ),
            event(
                4,
                collab::EventBody::DeliveryRequested {
                    message_seq: 2,
                    sender: bob.clone(),
                    recipient: carol.clone(),
                    kind: collab::MessageKind::Notice,
                },
            ),
            event(
                5,
                collab::EventBody::DeliveryAdvanced {
                    message_seq: 1,
                    recipient: bob.clone(),
                    state: State::Queued,
                    reason: None,
                },
            ),
            event(
                6,
                collab::EventBody::BindingChanged {
                    participant: bob.clone(),
                    credential: 7,
                    detached: false,
                },
            ),
            event(
                7,
                collab::EventBody::DeliveryRequested {
                    message_seq: 5,
                    sender: carol,
                    recipient: bob.clone(),
                    kind: collab::MessageKind::Notice,
                },
            ),
        ];
        assert_eq!(
            requested_for(&bob, &events),
            vec![1, 5],
            "carol's mail, our own receipts and a binding change are not deliveries"
        );
    }

    fn event(seq: u64, body: collab::EventBody) -> collab::ChannelEvent {
        collab::ChannelEvent { seq, at: seq, body }
    }

    fn party(byte: u8) -> collab::Party {
        collab::Party::Key(vec![byte; 32])
    }

    /// The generation on a delivery is the RECIPIENT's binding, not anything
    /// the sender said. Confusing the two hands a message aimed at a replaced
    /// attachment to the device that replaced it — the exact thing the daemon's
    /// fence is there to stop. And the body is chat's, flattened.
    #[test]
    fn a_delivery_is_fenced_by_the_recipients_binding_and_carries_chats_body() {
        let delivery = collab::Delivery {
            channel_id: CONVERSATION.into(),
            seq: 4,
            message_id: "m-3".into(),
            sender: party(1),
            recipient: party(2),
            kind: collab::MessageKind::Question,
            task: None,
            references: vec![collab::Reference::Blob { hash: "ab".into() }],
            expires_at: 900,
            requested_at: 50,
            state: State::Stored,
            advanced_by: 0,
            reason: None,
            updated_at: 50,
        };
        let message = chat::MessageView {
            channel_id: CONVERSATION.into(),
            seq: 4,
            head: chat::MessageHead {
                message_id: "m-3".into(),
                author: party(1),
                origin: sdk::Origin::External(vec![1; 32]),
                content_origin: sdk::Origin::External(vec![1; 32]),
                blocks: vec![
                    chat::Block::paragraph("ready?"),
                    chat::Block::Code {
                        lang: None,
                        text: "cargo test".into(),
                    },
                ],
                created_at: 40,
                rev: 0,
                revision: 1,
                edited_at: None,
                base_rev: None,
                deleted: false,
                thread: Some(2),
                reply_count: 0,
                last_reply_seq: None,
            },
        };
        let frame = deliver(&delivery, &message, 42);
        assert_eq!(frame.binding_generation, 42, "the recipient's binding");
        assert_eq!(frame.message_id, "m-3", "chat's id, verbatim");
        assert_eq!(frame.sender, handle(&party(1)));
        assert_eq!(frame.participant, handle(&party(2)));
        assert_eq!(frame.body, "ready?\ncargo test");
        assert_eq!(
            frame.reply_to,
            Some(2),
            "the thread root is the reply target"
        );
        assert_eq!(frame.expires_at, 900, "the network's deadline, unconverted");
        assert_eq!(frame.network_now, 50, "the agreed clock at the request");
        assert!(
            !frame.urgent,
            "no committed field asks for a turn to be steered"
        );
        assert_eq!(
            frame.references,
            vec![wire::Reference::Blob { hash: "ab".into() }]
        );
    }

    // The committed collaboration -> node -> daemon -> committed receipt
    // fixture lived here. It composed the NATIVE chat, tasks and collaboration
    // modules, which ship from ducktape-modules now, so nothing in this
    // workspace can stand that genesis up.
}
