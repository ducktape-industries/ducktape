//! `agent-service` — the execution half of the interactive terminal plane.
//!
//! This crate is what `ducktape service run agent` is made of. It owns the part
//! of a pty session that owns a PROCESS: resolving a provider tag, spawning the
//! sandboxed TUI, pumping its output, and tearing it down. It owns nothing
//! else — no scrollback ring, no ws topic, no mesh peer, no consensus query, no
//! admission decision. Those belong to the node, which drives this over
//! [`wire`].
//!
//! ## the boundary, and why it is here
//!
//! The pty plane splits at the pty, not at the session. Everything above the
//! pty is the node's by construction:
//!
//! - the scrollback and command rings are owned by the node's stream hub and
//!   read by its ws catch-up path;
//! - a pty client attaches to the NODE's `/v1/ws`, so `term:<id>` must be
//!   served there;
//! - cross-node sessions ride the mesh term plane, which authenticates peers by
//!   their mesh `PeerId` and answers admission from committed state on the
//!   node's actor lane — a daemon holds no keypair and no mesh identity.
//!
//! What is left below the pty is exactly this crate: a provider set, a bounded
//! map of live `InteractiveSession`s, one pump task each, and a wall-clock
//! reaper. That is also precisely the sandbox-touching part, which is the
//! point: after the carve the node process constructs no provider set and no
//! pty.
//!
//! ## the failure domain
//!
//! A session cannot outlive the connection that created it. When the link to
//! the node drops, [`Sessions::close_all`] ends every live session: the node
//! has already forgotten them (it cannot serve a topic it can no longer feed),
//! so a surviving pty would be an orphaned container nobody can reach or close.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use provider_host::{
    AirlockConfig, InteractiveSession, Provider, ProviderSet, ResolvedCredential, RunContext,
    WorkRef,
};
use tokio::sync::{mpsc, oneshot};

pub mod messaging;
pub mod wire;

/// the per-daemon concurrent-session cap. a terminal is arbitrary code
/// execution on the operator's host burning the operator's subscription, so the
/// ceiling is deliberately small; over it, create refuses rather than spawning.
pub const MAX_TERM_SESSIONS: usize = 4;

/// the hard wall-clock ceiling on any single session. A session is a human
/// driving a CLI TUI, so 4h is a generous single working session; past it the
/// session is force-closed no matter what. This is the backstop that makes a
/// session non-immortal: the primary teardown is still an explicit close, but
/// if the client is killed (its close never runs) or an idle TUI is just left
/// open, this timer guarantees the container + its slot are reclaimed instead of
/// pinned forever. There is deliberately NO idle-timeout — a terminal is
/// legitimately idle while a human reads; silence is not death.
const MAX_SESSION_LIFETIME: Duration = Duration::from_secs(4 * 60 * 60);

/// one pty read chunk. human typing + TUI redraws are modest; a chunk this size
/// coalesces a redraw burst into few frames without a large per-session buffer.
const TERM_READ_BUF: usize = 32 * 1024;

/// how many drive items (input + resize) a session's own lane may hold before
/// [`Sessions::enqueue`] refuses instead of queuing. The driver is one task
/// serializing writes to a pty that can stall (a TUI mid-render, a paused
/// process) for as long as its owner likes; without a ceiling here the lane
/// grows without limit at network speed, which is exactly the OOM this bound
/// exists to prevent.
const DRIVE_LANE_CAPACITY: usize = 64;

/// the per-session ceiling on bytes sitting in the drive lane, counted from
/// enqueue to the driver actually receiving the item. `DRIVE_LANE_CAPACITY`
/// alone bounds the FRAME count, not their size — a peer sending
/// `DRIVE_LANE_CAPACITY` maximal frames would still be tens of megabytes. This
/// is the second, size-based half of the same bound.
const MAX_PENDING_INPUT_BYTES: usize = 1024 * 1024;

/// how long the driver waits for one pty write before giving up on it and
/// moving to the next queued item. A write blocks for as long as the child
/// leaves its stdin undrained; without this, one stalled pty pins the driver
/// task forever and every later item on this session's lane never runs (the
/// lane itself may still be bounded, but the session it belongs to is dead in
/// the water either way).
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// the interactive-session execution plane. Arc-backed so a clone rides into
/// each session's pump and reaper task.
#[derive(Clone)]
pub struct Sessions(Arc<Inner>);

struct Inner {
    /// the sandbox-backed provider set. Unlike the in-node manager this is not
    /// optional: a daemon with no runnable sandbox refuses to start, so "no
    /// sandbox" is a state only the node can be in (no daemon attached).
    providers: ProviderSet,
    /// this host's canonical execution id — a run's directories are named from
    /// it, so two nodes sharing one user never collide.
    executing_node: String,
    /// per-session workdirs are created under here (the provider mounts one rw
    /// into the container; the fresh mount namespace fences the rest off), and
    /// removed with the session that owns them — see [`SessionHome`].
    workdir_root: PathBuf,
    /// live sessions. `std::sync::Mutex`: every critical section clones an `Arc`
    /// out and drops the guard before any `.await`, so it never crosses an await
    /// point.
    sessions: Mutex<HashMap<String, Live>>,
    /// reserved-or-live session count, the atomic backing the concurrency cap.
    /// reserved at create (before the spawn await), released exactly once when
    /// the session leaves the map.
    active: AtomicUsize,
    /// which link generation is current. Bumped by [`Sessions::close_all`] on
    /// every disconnect, and captured by a create BEFORE its spawn await: a pty
    /// that finishes starting under a stale epoch is one the node has already
    /// forgotten, so it is torn down instead of registered. Without this the
    /// sweep cannot see a session that is not in the map yet, and the container
    /// survives, unreachable, until the wall-clock ceiling.
    epoch: AtomicU64,
    /// Removed sessions can still own a PTY while asynchronous close runs.
    closing: AtomicUsize,
    closed: tokio::sync::Notify,
    /// the one way anything here reaches the node. Every writer goes through
    /// [`Inner::emit`], so frame ordering is owned by a single place.
    events: mpsc::Sender<wire::Event>,
}

/// a live session plus the drop-guards that end what the map entry owns. When
/// the entry leaves the map, dropping `_reaper_cancel` resolves the reaper's
/// cancel receiver, so its timer exits WITHOUT firing — an early end can never
/// leave a stale timer around to reap a later session that reused this id — and
/// dropping `_home` removes the session's workdir.
struct Live {
    session: Arc<InteractiveSession>,
    /// this session's ordered input lane. Its only long-lived sender, so
    /// dropping the map entry ends the driver task — the same drop-driven
    /// teardown the pump and reaper take. Bounded: see [`DRIVE_LANE_CAPACITY`].
    drive: mpsc::Sender<Drive>,
    /// bytes currently sitting in `drive`, from [`Sessions::enqueue`] to the
    /// driver's `recv`. Shared with the driver task so it can give back what
    /// it takes off the lane; see [`MAX_PENDING_INPUT_BYTES`].
    pending_input_bytes: Arc<AtomicUsize>,
    _reaper_cancel: oneshot::Sender<()>,
    /// declared LAST so it drops last: the container that mounts this directory
    /// is torn down by [`Sessions::finish`] before the entry is dropped at all.
    _home: SessionHome,
}

/// this session's workdir under [`Inner::workdir_root`] — created when the
/// session starts and REMOVED when it ends, on every exit path (an explicit
/// close, the pty's EOF, the wall-clock ceiling, a lost link, a spawn that never
/// got off the ground), because a Drop runs on all of them.
///
/// The session's whole host footprint is in there: the provider mounts it rw
/// into the container, the executor writes its state into it, and the run's
/// fresh config home sits inside it. Nothing else ever reaped it, so a node
/// accumulated one such tree for every pty session it had EVER hosted.
///
/// Removing it at session end loses nothing. A pty session is marked
/// `portable` — host-local, never resumed or captured — and its container is
/// destroyed by the same teardown, so once the session ends there is no longer
/// anything that can reach this directory.
///
/// A SIGKILLed daemon is the one death that leaves one standing, and that is
/// deliberately not swept: the node draws a session id at random, so no later
/// session can name a leftover and inherit it, and what survives is inert bytes
/// bounded by [`MAX_TERM_SESSIONS`] per kill. The part that is NOT inert — the
/// containers — is what the daemon's boot sweep already reaps.
struct SessionHome {
    dir: PathBuf,
}

impl SessionHome {
    /// materialize the session's workdir. The provider would `create_dir_all`
    /// it too, but doing it HERE puts both ends of the directory's life in the
    /// one place that owns [`Inner::workdir_root`] — and makes a spawn that
    /// fails before the provider ever runs still leave nothing behind.
    fn create(root: &Path, session: &str) -> Result<Self, String> {
        let dir = root.join(session);
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("create session workdir: {error}"))?;
        Ok(Self { dir })
    }

    fn path(&self) -> PathBuf {
        self.dir.clone()
    }
}

impl Drop for SessionHome {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            // the path is deliberately absent: it names a directory that held
            // the session's credential material. the session id is the handle.
            tracing::warn!(
                target: "ducktape::term",
                reason = "session_workdir_not_removed",
                %error,
                "the session workdir outlived its session"
            );
        }
    }
}

/// one thing to do to a live pty, in the order it arrived.
///
/// These ride a PER-SESSION lane rather than being performed on the link task.
/// A pty master write blocks whenever the child stops draining stdin — a TUI
/// mid-render, a paste larger than the tty buffer — and doing it inline would
/// make the link the serialization point for every session at once: one
/// blocked pty would stop all four sessions' output. Per-session ordering is
/// the requirement; a shared queue was never part of it.
enum Drive {
    Input(String),
    Resize { cols: u16, rows: u16 },
}

/// why [`Sessions::enqueue`] did not queue an item. The link surfaces
/// [`EnqueueRefusal::LaneFull`] as a warning; [`EnqueueRefusal::UnknownSession`]
/// is already warned inside this crate, where the lookup happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueRefusal {
    /// no live session by this id (already ended, or never existed).
    UnknownSession,
    /// the session's bounded drive lane, or its pending-byte budget, is full:
    /// the driver is not keeping up (a stalled pty). The item is DROPPED —
    /// never buffered — which is the whole point of the bound.
    LaneFull,
}

impl Sessions {
    /// build the plane. `executing_node` is this host's run-scoping id and
    /// `events` is the daemon's link to its node.
    pub fn new(
        providers: ProviderSet,
        executing_node: String,
        workdir_root: PathBuf,
        events: mpsc::Sender<wire::Event>,
    ) -> Self {
        Self(Arc::new(Inner {
            providers,
            executing_node,
            workdir_root,
            sessions: Mutex::new(HashMap::new()),
            active: AtomicUsize::new(0),
            epoch: AtomicU64::new(0),
            closing: AtomicUsize::new(0),
            closed: tokio::sync::Notify::new(),
            events,
        }))
    }

    /// THE dispatch. Every input the node can send is a named variant, and each
    /// arm is a single delegation to a handler named for it — so a new command
    /// fails the build until it is routed.
    ///
    /// Returns the enqueue refusal, when the command was one of the two that
    /// only enqueue — `TermCreate`/`TermClose` answer over the event lane
    /// instead, so they always return `None` here.
    pub async fn dispatch(&self, command: wire::Command) -> Option<EnqueueRefusal> {
        match command {
            wire::Command::TermCreate(create) => {
                self.create(create).await;
                None
            }
            wire::Command::TermInput { session, data_b64 } => {
                self.enqueue(&session, Drive::Input(data_b64))
            }
            wire::Command::TermResize {
                session,
                cols,
                rows,
            } => self.enqueue(&session, Drive::Resize { cols, rows }),
            wire::Command::TermClose { session } => {
                self.finish(&session).await;
                None
            }
            // the collaboration plane's, and routed there by
            // `messaging::route` before anything reaches here. Named rather
            // than swept into a `_` so a new command still fails the build
            // until somebody decides which plane owns it.
            command @ (wire::Command::MsgBind(_)
            | wire::Command::MsgUnbind { .. }
            | wire::Command::MsgDeliver(_)
            | wire::Command::MsgTime { .. }
            | wire::Command::MsgReplay { .. }) => {
                misrouted(&command);
                None
            }
        }
    }

    /// how many sessions are live — what the daemon's status line reports.
    pub fn live(&self) -> usize {
        self.0.active.load(Ordering::SeqCst)
    }

    /// end every live session, because the link that owned them is gone.
    ///
    /// The node forgets its sessions when a daemon detaches (it cannot feed a
    /// topic it can no longer reach), so a pty that survived the disconnect
    /// would be an orphaned container nobody can attach to, close, or reap
    /// until the wall-clock ceiling fires hours later. Called by the link task
    /// on every disconnect, before it redials.
    pub async fn close_all(&self) {
        // BEFORE the snapshot: a create still inside `spawn_interactive` is not
        // in the map yet, so the sweep below cannot see it. Bumping first is
        // what makes that create notice, on its way out, that the node it was
        // starting for has already forgotten it.
        self.0.epoch.fetch_add(1, Ordering::SeqCst);
        let live: Vec<String> = self
            .0
            .sessions
            .lock()
            .expect("agent sessions lock poisoned")
            .keys()
            .cloned()
            .collect();
        let has_live = !live.is_empty();
        if has_live {
            tracing::info!(
                target: "ducktape::term",
                sessions = live.len(),
                reason = "link_lost",
                "ending every session: the node connection dropped"
            );
        }
        for id in live {
            self.finish(&id).await;
        }
        loop {
            let closed = self.0.closed.notified();
            tokio::pin!(closed);
            closed.as_mut().enable();
            if self.0.closing.load(Ordering::SeqCst) == 0 {
                return;
            }
            closed.await;
        }
    }

    // ---- the handlers, one per command ------------------------------------

    /// spawn a session, then answer with exactly one terminal event. The
    /// decision and the write are separated: [`Self::try_create`] decides and
    /// touches no wire, this emits.
    async fn create(&self, spec: wire::Create) {
        let session = spec.session.clone();
        let provider = spec.provider.clone();
        let event = match self.try_create(spec).await {
            Ok(()) => {
                tracing::info!(target: "ducktape::term", session = %session, provider, "session_created");
                wire::Event::TermCreated { session }
            }
            Err((reason, detail)) => {
                // `detail` is the sentence that says WHY — the missing limit,
                // the absent artifact, the spawn's own error. The token alone
                // is unactionable, and this event is the only place the detail
                // is ever readable: it does not cross back to the caller.
                tracing::warn!(
                    target: "ducktape::term",
                    session = %session,
                    provider,
                    reason = reason.token(),
                    detail = %detail,
                    "session create refused"
                );
                wire::Event::TermRefused {
                    session,
                    reason,
                    detail,
                }
            }
        };
        self.0.emit(event).await;
    }

    /// resolve, reserve, spawn. Returns the refusal rather than emitting it, so
    /// the whole admission ladder is testable without a wire.
    async fn try_create(&self, spec: wire::Create) -> Result<(), (wire::Refusal, String)> {
        let provider = self
            .0
            .providers
            .resolve(&spec.provider)
            .map_err(|detail| (wire::Refusal::UnknownProvider, detail))?;
        // reserve BEFORE the spawn await, so two concurrent creates cannot both
        // slip past a stale count. Released on any failure below.
        let over_cap = self.0.active.fetch_add(1, Ordering::SeqCst) + 1 > MAX_TERM_SESSIONS;
        if over_cap {
            self.0.active.fetch_sub(1, Ordering::SeqCst);
            return Err((
                wire::Refusal::AtCapacity,
                format!("terminal session cap ({MAX_TERM_SESSIONS}) reached"),
            ));
        }
        match self.spawn(provider, spec).await {
            Ok(()) => Ok(()),
            Err(failure) => {
                self.0.active.fetch_sub(1, Ordering::SeqCst);
                Err(failure)
            }
        }
    }

    /// build the run context, spawn the pty, and register it with its pump and
    /// reaper. The cap reservation is held by [`Self::try_create`].
    async fn spawn(
        &self,
        provider: &dyn Provider,
        spec: wire::Create,
    ) -> Result<(), (wire::Refusal, String)> {
        // the id becomes a directory name below, so it is checked HERE, at the
        // boundary where it arrives, and not trusted for having come from our
        // own node.
        if !wire::valid_session(&spec.session) {
            return Err((
                wire::Refusal::SpawnFailed,
                "session id must be 16 lowercase hex".to_string(),
            ));
        }
        // captured before the await: see `Inner::epoch`.
        let epoch = self.0.epoch.load(Ordering::SeqCst);
        // built BEFORE the spawn below, so a spawn that fails takes its
        // half-materialized workdir down with it on the `?`.
        let home = SessionHome::create(&self.0.workdir_root, &spec.session)
            .map_err(|detail| (wire::Refusal::SpawnFailed, detail))?;
        let ctx = RunContext {
            agent_id: Some(spec.provider.clone()),
            // the executing-node id scopes this run's directories.
            executing_node: Some(self.0.executing_node.clone()),
            // a fresh per-session workdir, carried into the sandbox rw and
            // removed when `home` drops.
            workdir_override: Some(home.path()),
            limits: spec.limits,
            airlock: spec.credential.map(airlock_config),
            ..Default::default()
        };
        let session = provider
            .spawn_interactive(&ctx, spec.restricted)
            .await
            .map_err(|detail| (wire::Refusal::SpawnFailed, detail))?;
        let session = Arc::new(session);
        // the link dropped while this pty was starting. The node forgot this
        // session when it detached, so nothing will ever reach it, close it, or
        // read its output — registering it now would leak a container running
        // the agent CLI until the wall-clock ceiling.
        let node_forgot_it = self.0.epoch.load(Ordering::SeqCst) != epoch;
        if node_forgot_it {
            session.close().await;
            return Err((
                wire::Refusal::SpawnFailed,
                "the node link dropped while the session was starting".to_string(),
            ));
        }
        // dropping `cancel_tx` (when the entry leaves the map) cancels the
        // reaper; holding it in the map keeps the ceiling armed for the session.
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (drive, drive_rx) = mpsc::channel(DRIVE_LANE_CAPACITY);
        let pending_input_bytes = Arc::new(AtomicUsize::new(0));
        self.0
            .sessions
            .lock()
            .expect("agent sessions lock poisoned")
            .insert(
                spec.session.clone(),
                Live {
                    session: session.clone(),
                    drive,
                    pending_input_bytes: pending_input_bytes.clone(),
                    _reaper_cancel: cancel_tx,
                    _home: home,
                },
            );
        self.spawn_driver(
            spec.session.clone(),
            session.clone(),
            drive_rx,
            pending_input_bytes,
        );
        self.spawn_pump(spec.session.clone(), session);
        self.spawn_reaper(spec.session, cancel_rx);
        Ok(())
    }

    /// hand one input or resize to its session's own ordered lane.
    ///
    /// Non-blocking by construction — that is the whole point: `try_send`
    /// never awaits, so a stalled pty on session A can never delay session B,
    /// or the caller (the link, which must keep reading). An unknown id is a
    /// no-op + `warn` with a named reason, never a panic; a full lane or a
    /// blown byte budget is a DROP, never a buffer that grows to make room.
    fn enqueue(&self, id: &str, drive: Drive) -> Option<EnqueueRefusal> {
        let live = self
            .0
            .sessions
            .lock()
            .expect("agent sessions lock poisoned")
            .get(id)
            .map(|live| (live.drive.clone(), live.pending_input_bytes.clone()));
        let Some((lane, pending_input_bytes)) = live else {
            tracing::warn!(target: "ducktape::term", session = %id, reason = "unknown_session", "term drive dropped");
            return Some(EnqueueRefusal::UnknownSession);
        };
        // only `Input` carries a size worth budgeting; a resize is a fixed
        // couple of bytes and rides the frame-count bound alone.
        let input_len = match &drive {
            Drive::Input(data_b64) => Some(data_b64.len()),
            Drive::Resize { .. } => None,
        };
        if let Some(len) = input_len {
            let pending = pending_input_bytes.fetch_add(len, Ordering::SeqCst) + len;
            if pending > MAX_PENDING_INPUT_BYTES {
                pending_input_bytes.fetch_sub(len, Ordering::SeqCst);
                return Some(EnqueueRefusal::LaneFull);
            }
        }
        match lane.try_send(drive) {
            Ok(()) => None,
            Err(mpsc::error::TrySendError::Full(_)) => {
                if let Some(len) = input_len {
                    pending_input_bytes.fetch_sub(len, Ordering::SeqCst);
                }
                Some(EnqueueRefusal::LaneFull)
            }
            // the driver already exited (a teardown race with `finish`); the
            // session is ending, so the drop is benign and not a refusal the
            // caller needs to hear about.
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if let Some(len) = input_len {
                    pending_input_bytes.fetch_sub(len, Ordering::SeqCst);
                }
                None
            }
        }
    }

    /// the driver: one task per session, performing that session's inputs and
    /// resizes in the order they arrived.
    ///
    /// Serial per session IS the ordering guarantee. Doing this on the link task
    /// instead would have made a single blocked pty — a TUI mid-render, a paste
    /// bigger than the tty buffer — stop every other session's output too, since
    /// the link would not be reading while it waited.
    fn spawn_driver(
        &self,
        id: String,
        session: Arc<InteractiveSession>,
        mut lane: mpsc::Receiver<Drive>,
        pending_input_bytes: Arc<AtomicUsize>,
    ) {
        tokio::spawn(async move {
            while let Some(drive) = lane.recv().await {
                match drive {
                    Drive::Input(data_b64) => {
                        // the byte is off the lane the moment `recv` hands it
                        // over — give the budget back before the (possibly
                        // slow) write, not after, so a stalled write does not
                        // also pin the budget for bytes that already left the
                        // queue.
                        pending_input_bytes.fetch_sub(data_b64.len(), Ordering::SeqCst);
                        write_input(&id, &session, &data_b64).await;
                    }
                    Drive::Resize { cols, rows } => resize(&id, &session, cols, rows),
                }
            }
        });
    }

    // ---- the session lifetime ---------------------------------------------

    /// the pump: copy pty output to the node until EOF, then end the session.
    /// One task per session.
    fn spawn_pump(&self, id: String, session: Arc<InteractiveSession>) {
        let plane = self.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; TERM_READ_BUF];
            loop {
                match session.read(&mut buf).await {
                    // EOF: the child (and its container) is gone.
                    Ok(0) => break,
                    Ok(read) => {
                        // never log the bytes — only their count.
                        tracing::trace!(target: "ducktape::term", session = %id, bytes = read, "term_output");
                        plane
                            .0
                            .emit(wire::Event::TermOutput {
                                session: id.clone(),
                                chunk_b64: STANDARD.encode(&buf[..read]),
                            })
                            .await;
                    }
                    Err(error) => {
                        tracing::warn!(target: "ducktape::term", session = %id, reason = "read_failed", error = %error, "term pump stopped");
                        break;
                    }
                }
            }
            plane.finish(&id).await;
        });
    }

    /// arm the hard wall-clock ceiling. Cancelled cleanly the moment the session
    /// ends earlier: [`Self::finish`] drops the entry (and with it the cancel
    /// sender), the select takes the cancel arm, and the timer never fires — so
    /// it cannot reap a later session that reused this id.
    fn spawn_reaper(&self, id: String, cancel: oneshot::Receiver<()>) {
        let plane = self.clone();
        tokio::spawn(async move {
            if !reaper_fires(MAX_SESSION_LIFETIME, cancel).await {
                return; // the session ended before the ceiling — nothing to reap.
            }
            tracing::info!(target: "ducktape::term", session = %id, reason = "lifetime_ceiling", "session reaped");
            plane.finish(&id).await;
        });
    }

    /// end a session exactly once: remove it, release its slot, tell the node,
    /// tear the container down, and take its workdir with it.
    ///
    /// Whoever removes the entry owns the teardown, so an explicit close racing
    /// the pump's EOF can never double-terminate. The `TermEnded` frame is
    /// emitted BEFORE `close()` because the terminator is what unblocks an
    /// attached client and the container teardown can take seconds. The event
    /// retires the session at the receiver, which discards any late pump
    /// output. close_all separately waits for process and workdir teardown.
    ///
    /// The workdir goes LAST, and by construction rather than by a statement:
    /// `live` — and the [`SessionHome`] inside it — is dropped at the end of
    /// this function, which the awaited `close()` above already precedes. The
    /// directory the container mounts is never removed while it is mounted.
    async fn finish(&self, id: &str) {
        let removed = {
            let mut sessions = self
                .0
                .sessions
                .lock()
                .expect("agent sessions lock poisoned");
            let removed = sessions.remove(id);
            // Increment under the same lock as removal so close_all cannot
            // observe both an empty map and no outstanding teardown.
            if removed.is_some() {
                self.0.closing.fetch_add(1, Ordering::SeqCst);
            }
            removed
        };
        // Declared before live so even cancellation drops its workdir before
        // notifying shutdown waiters that teardown has finished.
        let _teardown = removed.as_ref().map(|_| Teardown(&self.0));
        let Some(live) = removed else {
            return; // already ended: idempotent by construction.
        };
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0
            .emit(wire::Event::TermEnded {
                session: id.to_string(),
            })
            .await;
        live.session.close().await;
        tracing::info!(target: "ducktape::term", session = %id, "session_ended");
    }
}

struct Teardown<'a>(&'a Inner);

impl Drop for Teardown<'_> {
    fn drop(&mut self) {
        self.0.closing.fetch_sub(1, Ordering::SeqCst);
        self.0.closed.notify_waiters();
    }
}

impl Inner {
    /// the ONE writer. A closed lane means the link task is gone and the process
    /// is shutting down; the pty dies with it, so the drop is benign and worth a
    /// debug line, never a warn per frame.
    async fn emit(&self, event: wire::Event) {
        if self.events.send(event).await.is_err() {
            tracing::debug!(
                target: "ducktape::term",
                reason = "link_closed",
                "agent event dropped"
            );
        }
    }
}

/// a collaboration command that reached the terminal plane. Only possible if a
/// caller bypassed [`messaging::route`], so it is a wiring bug, not traffic.
fn misrouted(command: &wire::Command) {
    let plane = match command {
        wire::Command::MsgBind(_) => "msg_bind",
        wire::Command::MsgUnbind { .. } => "msg_unbind",
        wire::Command::MsgDeliver(_) => "msg_deliver",
        wire::Command::MsgTime { .. } => "msg_time",
        wire::Command::MsgReplay { .. } => "msg_replay",
        wire::Command::TermCreate(_)
        | wire::Command::TermInput { .. }
        | wire::Command::TermResize { .. }
        | wire::Command::TermClose { .. } => "term",
    };
    tracing::warn!(
        target: "ducktape::term",
        reason = "misrouted_command",
        command = plane,
        "a collaboration command reached the terminal plane"
    );
}

/// write raw bytes to a pty. Bad base64 or a failed write is a no-op + `warn`
/// with a named reason — never a panic, and never the bytes in a log line.
///
/// Bounded by [`WRITE_TIMEOUT`]: `InteractiveSession::write_all` offers no
/// timeout of its own (it awaits the pty fd becoming writable, with no
/// ceiling), so a child that stops draining its stdin would otherwise pin
/// this driver task — and every later item on THIS session's lane — forever.
/// A dropped write may have landed partially; the child already saw a torn
/// keystroke sequence the moment its stdin stopped draining, so this loses
/// nothing an already-stalled pty had not already lost.
async fn write_input(id: &str, session: &InteractiveSession, data_b64: &str) {
    let Ok(bytes) = STANDARD.decode(data_b64) else {
        tracing::warn!(target: "ducktape::term", session = %id, reason = "bad_base64", "term input dropped");
        return;
    };
    match tokio::time::timeout(WRITE_TIMEOUT, session.write_all(&bytes)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(target: "ducktape::term", session = %id, reason = "write_failed", error = %error, "term input dropped");
        }
        Err(_elapsed) => {
            tracing::warn!(target: "ducktape::term", session = %id, reason = "write_stalled", "term input dropped: the pty did not drain in time");
        }
    }
}

/// set a pty's window size so the child's TUI reflows.
fn resize(id: &str, session: &InteractiveSession, cols: u16, rows: u16) {
    if let Err(error) = session.resize(cols, rows) {
        tracing::warn!(target: "ducktape::term", session = %id, reason = "resize_failed", error = %error, "term resize dropped");
    }
}

/// resolve to `true` iff `lifetime` elapses before the session is cancelled (its
/// reaper-cancel sender dropped). Split out so the ceiling-vs-cancel race is
/// unit-testable under paused time without a live pty.
async fn reaper_fires(lifetime: Duration, cancel: oneshot::Receiver<()>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(lifetime) => true,
        _ = cancel => false,
    }
}

/// Put a consensus-resolved credential record on the executor wire. The caller
/// queries committed state and the executor rebuilds it with [`airlock_config`].
/// Nothing secret crosses; see
/// [`wire::Credential`].
pub fn credential_wire(resolved: &ResolvedCredential) -> wire::Credential {
    wire::Credential {
        name: resolved.name.clone(),
        kind: resolved.kind,
        authority: resolved.authority.clone(),
        via: resolved.via.clone(),
        seal_pk: resolved.seal_pk,
    }
}

/// rebuild the broker's self-host airlock config from the record the node
/// resolved out of committed state. Nothing secret crosses — see [`wire`].
///
/// [`WorkRef::Direct`], and that is a statement rather than a default: an
/// interactive session has NO committed record of who asked for it, so there is
/// no pointer this side could offer and nothing a lender could resolve. The
/// subject stays the account the lender's node vouches for on the hop — the
/// node running this pty. Delegation is a saga-lane property because a saga is
/// the thing consensus wrote down.
fn airlock_config(credential: wire::Credential) -> AirlockConfig {
    AirlockConfig::self_host(
        &ResolvedCredential {
            name: credential.name,
            kind: credential.kind,
            authority: credential.authority,
            via: credential.via,
            seal_pk: credential.seal_pk,
        },
        WorkRef::Direct,
    )
}

/// build the sandbox-backed interactive provider set. Unlike the in-node
/// predecessor this returns the error instead of logging and degrading: a daemon
/// whose whole job is spawning ptys must not start without a provider set — it
/// would signal an interactive plane it cannot serve.
pub fn discover(
    node_identity: &[u8],
    capability_dir: &std::path::Path,
    backend: provider_host::SandboxBackend,
    owner: &str,
) -> Result<ProviderSet, String> {
    provider_host::discover(node_identity, capability_dir, None, backend, owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a provider that spawns a pty on a plain host `cat` — no VM, no broker,
    /// no guest image — so the session LIFECYCLE (which is what owns the
    /// workdir) is exercisable without a sandbox. `spawns = false` is the
    /// spawn-failure arm.
    struct StubProvider {
        spawns: bool,
    }

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn capability(&self) -> &str {
            "stub"
        }

        async fn run(&self, _prompt: &str, _ctx: &RunContext) -> Result<String, String> {
            Err("the stub provider is interactive-only".into())
        }

        async fn spawn_interactive(
            &self,
            _ctx: &RunContext,
            _restricted: bool,
        ) -> Result<InteractiveSession, String> {
            if !self.spawns {
                return Err("stub: no sandbox".into());
            }
            // `cat` sits on its pty until it is terminated — a live child with a
            // real master fd, which is all the session lifecycle needs.
            InteractiveSession::spawn_local(tokio::process::Command::new("cat"))
        }
    }

    const STUB_SESSION: &str = "00000000deadbeef";

    fn stub_create() -> wire::Create {
        wire::Create {
            session: STUB_SESSION.to_string(),
            provider: "stub".to_string(),
            restricted: false,
            limits: std::collections::BTreeMap::new(),
            credential: None,
        }
    }

    /// a `Sessions` over a scratch workdir root, plus the root and the event
    /// lane's receiver (held so `emit` never sees a closed channel).
    fn plane(name: &str) -> (Sessions, PathBuf, mpsc::Receiver<wire::Event>) {
        let root =
            std::env::temp_dir().join(format!("ducktape-term-home-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (events, rx) = mpsc::channel(8);
        let plane = Sessions::new(
            ProviderSet::empty(),
            "test-node".to_string(),
            root.clone(),
            events,
        );
        (plane, root, rx)
    }

    #[tokio::test]
    async fn a_session_workdir_dies_with_its_session() {
        // the leak this closes: every pty session a node hosted left
        // `<storage>/term-sessions/<id>` — its config home and whatever the
        // executor wrote — standing forever.
        let (plane, root, _rx) = plane("ends");
        let provider = StubProvider { spawns: true };
        plane
            .spawn(&provider, stub_create())
            .await
            .expect("the stub session spawns");
        let dir = root.join(STUB_SESSION);
        assert!(dir.is_dir(), "the session's workdir is materialized");

        // `finish` IS the teardown seam — the explicit close, the pump's EOF,
        // the wall-clock ceiling and a lost link all route through it.
        plane.finish(STUB_SESSION).await;
        assert!(
            !dir.exists(),
            "the session's workdir outlived the session: {}",
            dir.display()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn close_all_waits_for_teardown_already_removed_by_the_pump() {
        use std::task::Poll;
        let (plane, root, mut events) = plane("background-close");
        plane
            .spawn(&StubProvider { spawns: true }, stub_create())
            .await
            .unwrap();
        for _ in 0..8 {
            plane
                .0
                .events
                .send(wire::Event::TermOutput {
                    session: STUB_SESSION.into(),
                    chunk_b64: "eA==".into(),
                })
                .await
                .unwrap();
        }
        let finish = plane.finish(STUB_SESSION);
        tokio::pin!(finish);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(finish.as_mut().poll(cx).is_pending())).await
        );
        assert!(plane.0.sessions.lock().unwrap().is_empty());
        let close = plane.close_all();
        tokio::pin!(close);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(close.as_mut().poll(cx).is_pending())).await,
            "a removed record can still own a live PTY and workdir"
        );
        let drain = async {
            for _ in 0..9 {
                events.recv().await.unwrap();
            }
        };
        tokio::join!(finish, close, drain);
        assert!(!root.join(STUB_SESSION).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_spawn_that_fails_leaves_no_workdir_behind() {
        // the guard is built BEFORE the spawn, so the failure path has to take
        // the half-materialized directory with it — this session never reaches
        // the map, so `finish` will never run for it.
        let (plane, root, _rx) = plane("refused");
        let provider = StubProvider { spawns: false };
        let (refusal, _detail) = plane
            .spawn(&provider, stub_create())
            .await
            .expect_err("the stub refuses to spawn");
        assert_eq!(refusal, wire::Refusal::SpawnFailed);
        let dir = root.join(STUB_SESSION);
        assert!(
            !dir.exists(),
            "a refused create left a workdir behind: {}",
            dir.display()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn the_reaper_fires_when_nothing_cancels_it() {
        let (_tx, rx) = oneshot::channel();
        assert!(reaper_fires(Duration::from_secs(1), rx).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_cancel_sender_disarms_the_reaper() {
        // this is the "session ended early" path: `finish` drops the map entry,
        // which drops the sender. The ceiling must NOT fire afterwards, or it
        // would reap a later session that reused the id.
        let (tx, rx) = oneshot::channel::<()>();
        drop(tx);
        assert!(!reaper_fires(Duration::from_secs(1), rx).await);
    }

    /// register a `Live` entry directly, with its own drive lane, WITHOUT
    /// spawning a driver task — modeling a session whose driver never drains
    /// (a stalled pty), the exact condition this bound exists for. Returns the
    /// receiver too, so the caller controls whether/when anything is ever
    /// taken off the lane.
    fn register_stalled_session(plane: &Sessions, root: &Path) -> mpsc::Receiver<Drive> {
        let home = SessionHome::create(root, STUB_SESSION).expect("workdir");
        let session = Arc::new(
            InteractiveSession::spawn_local(tokio::process::Command::new("cat"))
                .expect("stub pty spawns"),
        );
        let (drive, drive_rx) = mpsc::channel(DRIVE_LANE_CAPACITY);
        let (cancel_tx, _cancel_rx) = oneshot::channel();
        plane
            .0
            .sessions
            .lock()
            .expect("agent sessions lock poisoned")
            .insert(
                STUB_SESSION.to_string(),
                Live {
                    session,
                    drive,
                    pending_input_bytes: Arc::new(AtomicUsize::new(0)),
                    _reaper_cancel: cancel_tx,
                    _home: home,
                },
            );
        drive_rx
    }

    #[tokio::test]
    async fn a_session_whose_driver_never_drains_refuses_once_its_lane_fills() {
        // the OOM this bound exists for: a peer that keeps sending while the
        // driver is stuck must hit a ceiling, not grow the lane forever.
        let (plane, root, _rx) = plane("lane-full");
        let _drive_rx = register_stalled_session(&plane, &root);

        // frames small enough that the FRAME-COUNT bound (not the byte
        // budget) is what refuses here.
        let small_frame = "x".repeat(8);
        let mut accepted = 0usize;
        loop {
            match plane.enqueue(STUB_SESSION, Drive::Input(small_frame.clone())) {
                None => accepted += 1,
                Some(EnqueueRefusal::LaneFull) => break,
                Some(EnqueueRefusal::UnknownSession) => panic!("the session is registered"),
            }
        }
        assert_eq!(accepted, DRIVE_LANE_CAPACITY);
        // it keeps refusing rather than ever buffering past the ceiling.
        for _ in 0..5 {
            assert_eq!(
                plane.enqueue(STUB_SESSION, Drive::Input(small_frame.clone())),
                Some(EnqueueRefusal::LaneFull)
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_session_whose_driver_never_drains_refuses_on_the_byte_budget_too() {
        // frame-count alone does not bound MEMORY: a handful of large frames
        // must refuse well before `DRIVE_LANE_CAPACITY` frames have queued.
        let (plane, root, _rx) = plane("byte-budget");
        let _drive_rx = register_stalled_session(&plane, &root);

        let big_frame = "y".repeat(200_000); // 1 MiB budget / 200 KiB = 5
        let mut accepted = 0usize;
        loop {
            match plane.enqueue(STUB_SESSION, Drive::Input(big_frame.clone())) {
                None => accepted += 1,
                Some(EnqueueRefusal::LaneFull) => break,
                Some(EnqueueRefusal::UnknownSession) => panic!("the session is registered"),
            }
        }
        assert!(
            accepted < DRIVE_LANE_CAPACITY,
            "the byte budget must bind before the frame count does: accepted {accepted}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_draining_session_accepts_more_than_the_lane_capacity_over_time() {
        // the lane's bound is on what is QUEUED, not on a session's lifetime
        // total — a session whose driver keeps up must never be refused just
        // because it has been running a while.
        let (plane, root, mut events) = plane("drains");
        let provider = StubProvider { spawns: true };
        plane
            .spawn(&provider, stub_create())
            .await
            .expect("the stub session spawns");

        let frames = DRIVE_LANE_CAPACITY * 2;
        for i in 0..frames {
            let data_b64 = STANDARD.encode(format!("{i}\n").into_bytes());
            let refusal = plane
                .dispatch(wire::Command::TermInput {
                    session: STUB_SESSION.to_string(),
                    data_b64,
                })
                .await;
            assert_eq!(refusal, None, "a draining session must never refuse");
            // wait for `cat`'s echo before sending the next byte: this is what
            // proves the driver actually drained it, rather than racing a
            // still-full lane — synchronized on the event, not a sleep.
            let event = events.recv().await.expect("the event lane stays open");
            assert!(matches!(event, wire::Event::TermOutput { .. }));
        }

        plane.finish(STUB_SESSION).await;
        let _ = std::fs::remove_dir_all(&root);
    }
}
