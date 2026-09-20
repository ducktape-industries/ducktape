//! the node ↔ agent-daemon service link.
//!
//! One local daemon (`ducktape service run agent`) dials this node's `/v1/ws`
//! and takes the link by presenting the node's 0600 workspace secret. What
//! rides it is the collaboration messaging bus: the node sends `Msg*` commands
//! down, the daemon reports `Msg*` receipts back, and the node's collaboration
//! pump turns each receipt into an on-chain `Acknowledge`.
//!
//! This node hosts no interactive terminal. A terminal session is an
//! independently installed `ducktape-terminal` process reached through its
//! signed gateway route — nothing here spawns, drives, mirrors or interprets
//! one, and the daemon link carries no terminal command.
//!
//! The link secret is also the node's ONE proof that a ws caller can read its
//! own workspace, which is what the workspace-gated stream topics
//! (`crate::stream::Admission::Workspace`) stand on. Holding the link strictly
//! contains holding a gated topic, so one file serves both and there is no
//! second secret to leak.

use std::sync::{Arc, Mutex};

use agent_service::wire;
use tokio::sync::mpsc;

/// how many commands may be in flight to the agent daemon before a sender
/// waits. Deep enough that a burst never blocks the ws reader; bounded so a
/// wedged daemon back-pressures instead of growing without limit.
const COMMAND_LANE: usize = 1024;

/// the node's half of the daemon link. Arc-backed so a clone rides onto the
/// [`crate::NodeHandle`]; injected as an `Option` (absent on a sync-only node).
///
/// Nothing here spawns a process. Every effect is a [`wire::Command`] down the
/// link, and every fact arrives as a [`wire::Event`] through [`Self::on_event`].
#[derive(Clone)]
pub struct ServiceLink(Arc<Link>);

struct Link {
    /// the attached agent daemon's command lane. `None` — no daemon
    /// signaling — is the state in which this node has no daemon to talk to.
    link: Mutex<Option<mpsc::Sender<wire::Command>>>,
    /// the secret a daemon must present to take the link. `None` — a node with
    /// no workspace to hold one — refuses every attach: holding the link means
    /// becoming this node's messaging plane, which is not a capability to hand
    /// out on the strength of dialing loopback.
    link_token: Option<String>,
    /// where the collaboration receipts go. `OnceLock` because the pump is
    /// wired once at boot and never replaced: a second setter would be two
    /// consumers racing for one receipt.
    collab: std::sync::OnceLock<mpsc::Sender<wire::Event>>,
    /// how many daemons have taken the link. See [`ServiceLink::attach_epoch`].
    attaches: std::sync::atomic::AtomicU64,
    /// how many receipts the pump was too slow to take. See
    /// [`ServiceLink::dropped_receipts`].
    dropped: std::sync::atomic::AtomicU64,
}

/// holds a daemon's attachment open. Dropping it — the ws connection closed,
/// the daemon exited, the node is shutting down — detaches the link.
pub struct AttachGuard(ServiceLink);

impl Drop for AttachGuard {
    fn drop(&mut self) {
        self.0.detach();
    }
}

/// the `ducktape::service` warns below that a CLIENT can repeat within one
/// daemon session: an attach arrives over a plain ws message any local process
/// can loop, and the real daemon redials a refused link every 2s forever. First
/// occurrence, then every 100th, carrying `occurrences`.
static LINK_WARN: crate::log::Latch = crate::log::Latch::new(100);

impl ServiceLink {
    /// build the link. No daemon is attached yet — one arrives (or does not)
    /// over the ws.
    pub fn new(link_token: Option<String>) -> Self {
        Self(Arc::new(Link {
            link: Mutex::new(None),
            link_token,
            collab: std::sync::OnceLock::new(),
            attaches: std::sync::atomic::AtomicU64::new(0),
            dropped: std::sync::atomic::AtomicU64::new(0),
        }))
    }

    /// Route the daemon's collaboration receipts to the node's collaboration
    /// pump. Until this is called they are dropped with a named reason
    /// ([`unconsumed`]) — a node can run a daemon link and no collaboration
    /// plane at all.
    ///
    /// Returns whether the lane was taken. A second call is refused rather than
    /// silently ignored: two consumers of one receipt would each submit an
    /// `Acknowledge` for it, and the second would be refused on-chain for a
    /// transition already made — a confusing failure with no local cause.
    pub fn route_collab_to(&self, lane: mpsc::Sender<wire::Event>) -> bool {
        self.0.collab.set(lane).is_ok()
    }

    // ---- the daemon's attachment ------------------------------------------

    /// Take the messaging plane for this connection.
    ///
    /// `None` when a daemon is already attached: one agent service per node, and
    /// FIRST ATTACH WINS. That is a boundary, not a nicety — a second attacher
    /// could otherwise displace the live daemon and receive the deliveries meant
    /// for it.
    pub fn attach(&self, token: &str) -> Option<(AttachGuard, mpsc::Receiver<wire::Command>)> {
        // two reasons, not one: "this node minted no secret" is an operator's
        // node to fix and "you presented the wrong one" is the daemon's, and
        // collapsing them sends whoever reads the log to the wrong machine.
        if self.0.link_token.is_none() {
            if let Some(occurrences) = LINK_WARN.hit("no_link_token") {
                tracing::warn!(
                    target: "ducktape::service",
                    reason = "no_link_token",
                    occurrences,
                    "agent service link refused"
                );
            }
            return None;
        }
        if !self.link_token_matches(token) {
            if let Some(occurrences) = LINK_WARN.hit("bad_link_token") {
                tracing::warn!(
                    target: "ducktape::service",
                    reason = "bad_link_token",
                    occurrences,
                    "agent service link refused"
                );
            }
            return None;
        }
        let mut link = self.0.link.lock().expect("service link lock poisoned");
        if link.is_some() {
            return None;
        }
        let (tx, rx) = mpsc::channel(COMMAND_LANE);
        *link = Some(tx);
        // a NEW daemon, and this is what says so. See [`Self::attach_epoch`].
        self.0
            .attaches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tracing::info!(target: "ducktape::service", "agent service attached");
        Some((AttachGuard(self.clone()), rx))
    }

    /// How many daemons have attached over this node's life. `0` = none ever.
    ///
    /// The collaboration pump caches what it has already told the daemon —
    /// which bindings, which clock, which retention floor — and a RESTARTED
    /// daemon knows none of it. "Is one attached" cannot answer that: a daemon
    /// that dies and redials with the same bindings looks identical to one that
    /// never left, and the pump would then never re-send a bind, leaving a live
    /// binding on the network that this node's daemon has never heard of.
    ///
    /// A counter and not a flag, because the pump may not observe the gap: a
    /// detach and a re-attach between two sweeps is invisible to any state that
    /// only says "attached now".
    pub fn attach_epoch(&self) -> u64 {
        self.0.attaches.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How many collaboration receipts this node dropped for a full pump lane.
    ///
    /// Also a counter and not a flag, and for a sharper reason than
    /// [`Self::attach_epoch`]'s: the pump only observes this AFTER it has
    /// drained enough of the lane to run a sweep, by which point a flag it
    /// consumed would race the next overflow. A monotonic count it compares
    /// against what it last saw cannot lose a drop, only coalesce several into
    /// one replay — which is all one replay costs.
    pub fn dropped_receipts(&self) -> u64 {
        self.0.dropped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Does `presented` match this node's 0600 workspace link secret?
    ///
    /// The node's ONE proof that a caller can read its own workspace, so it
    /// answers two questions: may you take the daemon link ([`Self::attach`]),
    /// and may you hold a workspace-gated ws topic
    /// (`crate::stream::Admission::Workspace`, reached through
    /// [`crate::NodeHandle::workspace_secret_matches`]). Holding the secret
    /// already grants the first, which strictly contains the second, so serving
    /// both from one file adds no authority — a second secret would only be a
    /// second thing to leak.
    ///
    /// `None` — a node with no workspace to hold one — matches NOTHING, which
    /// fails closed.
    ///
    /// A pure predicate: it logs nothing, because its two callers must not log
    /// alike. An attach arrives over a plain ws client message that a client
    /// (or a redialing agent daemon) can loop, so its `warn` is latched by
    /// [`LINK_WARN`]; a subscribe is per-request and locally drivable in a
    /// loop too, and it does not warn at all — a rejected subscription just
    /// answers with an `unavailable` frame. Each caller names its own level.
    pub(crate) fn link_token_matches(&self, presented: &str) -> bool {
        self.0
            .link_token
            .as_deref()
            .is_some_and(|expected| crate::services::token_matches(presented, expected))
    }

    /// drop the link. See [`AttachGuard`].
    fn detach(&self) {
        *self.0.link.lock().expect("service link lock poisoned") = None;
        tracing::info!(target: "ducktape::service", "agent service detached");
    }

    /// whether an agent service is attached.
    pub fn attached(&self) -> bool {
        self.link().is_some()
    }

    fn link(&self) -> Option<mpsc::Sender<wire::Command>> {
        self.0
            .link
            .lock()
            .expect("service link lock poisoned")
            .clone()
    }

    // ---- events from the daemon -------------------------------------------

    /// THE dispatch for everything the daemon reports. One arm per variant, each
    /// a single delegation — a new event fails the build until it is routed.
    pub fn on_event(&self, event: wire::Event) {
        match event {
            wire::Event::MsgBound { .. }
            | wire::Event::MsgBindRefused { .. }
            | wire::Event::MsgDelivery { .. } => self.receipt(event),
            // no terminal command ever leaves this node, so no terminal event
            // can be an answer to one. A daemon reporting one is skewed — it
            // was built against a tree this node is not.
            wire::Event::TermCreated { .. }
            | wire::Event::TermRefused { .. }
            | wire::Event::TermOutput { .. }
            | wire::Event::TermEnded { .. } => Self::unsolicited(),
        }
    }

    /// hand one collaboration receipt to the pump.
    ///
    /// `try_send` and not `send`: this runs on the ws READ LOOP. Awaiting a full
    /// pump lane here would stall the link behind one slow chain submission, so
    /// a full lane drops the receipt instead.
    ///
    /// A dropped receipt is NOT recoverable by re-reading the chain: the module
    /// records what was submitted, and this receipt is precisely the one that
    /// never was. So the drop is COUNTED ([`Self::dropped_receipts`]) and the
    /// pump asks the daemon — whose delivery journal is durable and is the only
    /// remaining witness — to replay it.
    fn receipt(&self, event: wire::Event) {
        let Some(lane) = self.0.collab.get() else {
            return unconsumed(&event);
        };
        if lane.try_send(event).is_err() {
            self.0
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(occurrences) = LINK_WARN.hit("collab_lane_full") {
                tracing::warn!(
                    target: "ducktape::collab",
                    reason = "collab_lane_full",
                    occurrences,
                    "dropped a collaboration receipt: the pump is not keeping up"
                );
            }
        }
    }

    fn unsolicited() {
        if let Some(occurrences) = LINK_WARN.hit("terminal_event") {
            tracing::warn!(
                target: "ducktape::service",
                reason = "terminal_event",
                occurrences,
                "dropped a terminal event: this node drives no terminal sessions"
            );
        }
    }

    /// THE single writer to the daemon.
    ///
    /// A missing or closed link drops the command with a named reason — never a
    /// panic. A drop is survivable: the module holds the authoritative record
    /// and the pump re-reads it.
    pub async fn send(&self, command: wire::Command) {
        let Some(link) = self.link() else {
            if let Some(occurrences) = LINK_WARN.hit("no_agent_service") {
                tracing::warn!(
                    target: "ducktape::service",
                    reason = "no_agent_service",
                    occurrences,
                    "service command dropped"
                );
            }
            return;
        };
        if link.send(command).await.is_err()
            && let Some(occurrences) = LINK_WARN.hit("agent_service_gone")
        {
            tracing::warn!(
                target: "ducktape::service",
                reason = "agent_service_gone",
                occurrences,
                "service command dropped"
            );
        }
    }
}

/// a collaboration receipt reached a node with no collaboration pump wired.
///
/// It is DROPPED, and said so: the daemon's delivery record is durable and the
/// module holds the authoritative receipt, so nothing is lost by not consuming
/// one here — but a receipt going nowhere silently is exactly the kind of gap
/// that gets discovered from a delivery that never appears to advance.
fn unconsumed(event: &wire::Event) {
    let kind = match event {
        wire::Event::MsgBound { .. } => "msg_bound",
        wire::Event::MsgBindRefused { .. } => "msg_bind_refused",
        wire::Event::MsgDelivery { .. } => "msg_delivery",
        wire::Event::TermCreated { .. }
        | wire::Event::TermRefused { .. }
        | wire::Event::TermOutput { .. }
        | wire::Event::TermEnded { .. } => "term",
    };
    if let Some(occurrences) = LINK_WARN.hit("collab_receipt_unconsumed") {
        tracing::warn!(
            target: "ducktape::collab",
            reason = "collab_receipt_unconsumed",
            event = kind,
            occurrences,
            "dropped a collaboration receipt: this node has no collaboration plane"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_link_needs_this_nodes_token() {
        // holding the link means BECOMING this node's messaging plane and
        // receiving every delivery with it. Dialing loopback is not enough.
        let link = ServiceLink::new(Some(TEST_TOKEN.into()));
        assert!(link.attach("").is_none(), "an empty token is not a token");
        assert!(
            link.attach("0123456789abcdef0123456789abcdee").is_none(),
            "a near miss is still a miss"
        );
        assert!(link.attach(TEST_TOKEN).is_some());
    }

    #[test]
    fn a_node_that_could_not_mint_a_token_refuses_every_attach() {
        // fail CLOSED: a node that cannot write its 0600 token has no way to
        // tell a daemon from any other local process, so it has no daemon plane
        // rather than an unguarded one.
        let link = ServiceLink::new(None);
        assert!(link.attach("").is_none());
        assert!(link.attach(TEST_TOKEN).is_none());
        assert!(!link.attached());
    }

    #[test]
    fn a_second_attach_cannot_displace_a_live_daemon() {
        // FIRST ATTACH WINS is a boundary: a local impersonator that could take
        // the link would receive the deliveries meant for the real daemon.
        let link = ServiceLink::new(Some(TEST_TOKEN.into()));
        let first = link.attach(TEST_TOKEN);
        assert!(first.is_some());
        assert!(
            link.attach(TEST_TOKEN).is_none(),
            "the link is already held"
        );
        // and it is reclaimable once the holder goes.
        drop(first);
        assert!(link.attach(TEST_TOKEN).is_some());
    }

    #[tokio::test]
    async fn a_detached_link_drops_commands_instead_of_panicking() {
        let link = ServiceLink::new(Some(TEST_TOKEN.into()));
        let (guard, mut rx) = link.attach(TEST_TOKEN).expect("the first attach wins");
        link.send(wire::Command::MsgTime { network_now: 7 }).await;
        assert_eq!(
            rx.recv().await,
            Some(wire::Command::MsgTime { network_now: 7 })
        );
        drop(guard);
        assert!(!link.attached());
        link.send(wire::Command::MsgTime { network_now: 8 }).await;
    }

    /// The receipts the pump exists for reach it; a terminal event — which only
    /// a daemon built against another tree can send — never does.
    #[tokio::test]
    async fn only_messaging_receipts_reach_the_collaboration_pump() {
        let link = ServiceLink::new(Some(TEST_TOKEN.into()));
        let (lane, mut receipts) = mpsc::channel(8);
        assert!(link.route_collab_to(lane));
        assert!(
            !link.route_collab_to(mpsc::channel(1).0),
            "one consumer per receipt"
        );
        link.on_event(wire::Event::TermEnded {
            session: "0000000000000001".into(),
        });
        let bound = wire::Event::MsgBound {
            conversation: "c".into(),
            participant: "p".into(),
            generation: 1,
            capabilities: wire::Capabilities {
                accepts_while_busy: true,
                wakes_idle: true,
                reports_acceptance: true,
                steers_active_turn: true,
            },
        };
        link.on_event(bound.clone());
        assert_eq!(receipts.recv().await, Some(bound));
        assert_eq!(link.dropped_receipts(), 0);
    }
}
