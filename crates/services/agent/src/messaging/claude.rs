//! the Claude Code adapter: one named local session's per-session inbox.
//!
//! ## the interface, as the installed build actually behaves
//!
//! An interactive session binds a unix socket and publishes a registry entry
//! naming it. Both are per-session and per-user:
//!
//! - `<sockets dir>/<pid>.sock`, mode 0600 in a 0700 directory, the sockets
//!   dir being `$XDG_RUNTIME_DIR/cc-socks` where a runtime dir is set and
//!   `<temp dir>/cc-socks` where not (`/tmp/cc-socks-<uid>` when a socket
//!   path there would not fit a socket address);
//! - `<claude home>/sessions/<pid>.json` — `{pid, sessionId, messagingSocketPath, …}`;
//! - `<claude home>/sessions/<pid>.<sha256(socket path)>.key`, mode 0600 —
//!   `{peerToken, …}`.
//!
//! The socket speaks newline-delimited JSON, client to server only. An auth
//! line first, then frames. **The server never writes on the connection.**
//!
//! ## why this registers ONE session instead of scanning
//!
//! A binding names an immutable `sessionId`. [`resolve`] reads the registry and
//! takes the single entry carrying it — and refuses if two do. It never
//! enumerates the sockets in the directory and attaches to what it finds:
//! every session in there belongs to the same operating-system user, and
//! "everything this user is running" is not a thing anyone consented to
//! receive messages on. The `session_id` also goes out ON the frame, where the
//! RECIPIENT re-checks it and drops a mismatch, so a pid that has been reused
//! by a different session cannot be written into by accident.
//!
//! ## why a write is not an acceptance
//!
//! There is no acknowledgement of any kind on the socket. A message that
//! routes to the session's queue is silent, and so is one dropped for a
//! session-id mismatch or refused by the session's own inbound policy. Write
//! success therefore proves the SOCKET took the bytes and nothing about the
//! session.
//!
//! What does exist is a back-channel: a sender that supplies a `from` address
//! receives `{"type":"control","action":"peer_message_status",…}` frames for
//! `held | denied | expired | delivered | refused | dropped`. There is
//! deliberately no positive `accepted` status, so:
//!
//! - a status arrives → [`Outcome`] carries what it said;
//! - no status arrives → [`Outcome::Unknown`]. Never `Accepted`.
//!
//! [`Receipts`] is what makes the first case possible, and it is optional by
//! construction: if this daemon cannot bind its own address, every delivery
//! settles `DeliveryUnknown` instead. Degrading is correct here — the
//! alternative is inventing an acceptance nobody reported.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::{Offer, Outcome};
use crate::wire::Capabilities;

/// how long to wait for the recipient's verdict before calling it unknown.
///
/// This bounds a REFUSAL, not a success: `held`, `denied` and `refused` are the
/// recipient's immediate decisions and arrive at socket latency, while an
/// accepted message is silent forever. Waiting longer would not turn silence
/// into an acceptance; it would only delay reporting the uncertainty.
const VERDICT_WINDOW: Duration = Duration::from_millis(1_500);

/// how long to keep listening after a message is HELD.
///
/// Long, because the thing being waited on is a human deciding — an approval
/// barrier holds a message until somebody looks at it. Bounded all the same,
/// because each held message costs a task and a map entry, and a session that
/// went away never says anything again.
///
/// It is a local resource bound and NOT a deadline: the message's real deadline
/// is `expires_at` on the agreed network clock. Nothing here converts one into
/// the other.
const HOLD_WINDOW: Duration = Duration::from_secs(30 * 60);

/// how many verdicts one message's lane holds. A message draws at most two —
/// the hold, then its resolution — and a peer sending more than a small handful
/// about one message has stopped making sense.
const VERDICTS_PER_MESSAGE: usize = 4;

/// one outstanding delivery's verdict lane, as [`Receipts`] holds it.
type Verdicts = tokio::sync::mpsc::Sender<Status>;

/// one resolved session: everything needed to reach it, and nothing else.
///
/// `token` is a secret. It is never logged, never rendered into an error, and
/// never leaves this process — the [`std::fmt::Debug`] impl below is what
/// makes that a property of the type rather than a habit.
pub struct Attached {
    pub pid: u32,
    socket: PathBuf,
    token: String,
    session_id: String,
}

impl std::fmt::Debug for Attached {
    /// deliberately partial: a session's pid is diagnosable, its socket path
    /// and inbox token are not. A `{:?}` in a log line or an error must not be
    /// the way either escapes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attached")
            .field("pid", &self.pid)
            .field("socket", &"<redacted>")
            .field("token", &"<redacted>")
            .finish()
    }
}

/// the readable half of a session's registry entry.
#[derive(Debug, Deserialize)]
struct RegistryEntry {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "messagingSocketPath")]
    messaging_socket_path: Option<String>,
}

/// the 0600 key file beside it.
#[derive(Debug, Deserialize)]
struct KeyFile {
    #[serde(rename = "peerToken")]
    peer_token: String,
}

/// why a session could not be reached. A stable token; never a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreachable {
    /// no registry entry carries that session id.
    NoSuchSession,
    /// more than one does. Ambiguity is refused rather than resolved by
    /// picking one — the spec's "ambiguous names return candidates and do not
    /// pick the first match", applied to the local half.
    Ambiguous,
    /// the entry exists but names no inbox (messaging is off there).
    NoInbox,
    /// the inbox key is absent or unreadable, so nothing could authenticate.
    NoKey,
}

impl Unreachable {
    pub fn token(self) -> &'static str {
        match self {
            Unreachable::NoSuchSession => "no_such_session",
            Unreachable::Ambiguous => "ambiguous_session",
            Unreachable::NoInbox => "no_inbox",
            Unreachable::NoKey => "no_key",
        }
    }
}

/// resolve ONE named session out of the registry directory.
pub async fn resolve(registry: &Path, session_id: &str) -> Result<Attached, Unreachable> {
    let mut dir = tokio::fs::read_dir(registry)
        .await
        .map_err(|_| Unreachable::NoSuchSession)?;
    let mut found: Option<RegistryEntry> = None;
    while let Ok(Some(item)) = dir.next_entry().await {
        let path = item.path();
        let is_entry = path.extension().is_some_and(|ext| ext == "json");
        if !is_entry {
            continue;
        }
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        let Ok(entry) = serde_json::from_str::<RegistryEntry>(&text) else {
            continue;
        };
        if entry.session_id != session_id {
            continue;
        }
        if found.is_some() {
            return Err(Unreachable::Ambiguous);
        }
        found = Some(entry);
    }
    let entry = found.ok_or(Unreachable::NoSuchSession)?;
    let socket = entry.messaging_socket_path.ok_or(Unreachable::NoInbox)?;
    let key_path = registry.join(key_file_name(entry.pid, &socket));
    let key_text = tokio::fs::read_to_string(&key_path)
        .await
        .map_err(|_| Unreachable::NoKey)?;
    let key: KeyFile = serde_json::from_str(&key_text).map_err(|_| Unreachable::NoKey)?;
    Ok(Attached {
        pid: entry.pid,
        socket: PathBuf::from(socket),
        token: key.peer_token,
        session_id: entry.session_id,
    })
}

/// `<pid>.<sha256(socket path)>.key` — the name the session publishes its inbox
/// key under. Derived rather than globbed on `<pid>.*.key`, so a session that
/// has moved its socket aside cannot have a stale key picked up for its new one.
fn key_file_name(pid: u32, socket: &str) -> String {
    let digest = Sha256::digest(socket.as_bytes());
    format!("{pid}.{digest:x}.key")
}

/// the adapter over one attached session.
pub struct ClaudeInbox {
    attached: Attached,
    receipts: Option<Arc<Receipts>>,
}

impl ClaudeInbox {
    pub fn new(attached: Attached, receipts: Option<Arc<Receipts>>) -> Self {
        Self { attached, receipts }
    }

    /// what an attached Claude session can do.
    ///
    /// `reports_acceptance` is `false` even WITH a receipt inbox, and that is
    /// the honest reading rather than a pessimistic one: the back-channel
    /// reports holds and refusals, never an acceptance. A sender that reads
    /// this field learns that `DeliveryUnknown` from this binding is the
    /// expected outcome of a delivery that went fine, not a fault to chase.
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            accepts_while_busy: true,
            wakes_idle: true,
            reports_acceptance: false,
            steers_active_turn: false,
        }
    }

    /// offer one message to the session's inbox.
    ///
    /// The second half of the answer is a [`Follow`]: this adapter's ONE
    /// acceptance signal is a held message being released, which arrives after
    /// the first verdict and after this returns. It is `Some` exactly when the
    /// message is being held and there is still a lane to hear the release on —
    /// every other path takes the waiter back out here, because a waiter
    /// registered before the write and never consumed would sit in the map for
    /// the life of the process.
    pub async fn offer(&self, offer: &Offer<'_>) -> (Outcome, Option<Follow>) {
        let correlation = correlation(offer.message_id);
        // registered BEFORE the write: a verdict can come back the moment the
        // recipient reads the line, and a waiter installed afterwards would
        // race it and read silence.
        let verdicts = self
            .receipts
            .as_ref()
            .map(|receipts| receipts.expect(&correlation));
        let reply_to = self
            .receipts
            .as_ref()
            .map(|receipts| format!("uds:{}", receipts.address().display()));
        let frames = self.frames(offer, &correlation, reply_to.as_deref());

        let (outcome, verdicts) = self.write_and_wait(&frames, verdicts).await;
        let holding = matches!(outcome, Outcome::Held { .. });
        let follow = match (holding, verdicts, &self.receipts) {
            (true, Some(verdicts), Some(receipts)) => Some(Follow {
                verdicts,
                receipts: receipts.clone(),
                correlation,
            }),
            // not held, or nothing left to listen on. The registration goes now.
            (_, _, receipts) => {
                if let Some(receipts) = receipts {
                    receipts.forget(&correlation);
                }
                None
            }
        };
        (outcome, follow)
    }

    /// write the frames, then wait for whatever the recipient says about them.
    ///
    /// Hands the verdict lane back so a held message can keep listening on the
    /// SAME registration — re-registering after the fact would lose a release
    /// that landed in between.
    async fn write_and_wait(
        &self,
        frames: &[String],
        verdicts: Option<tokio::sync::mpsc::Receiver<Status>>,
    ) -> (Outcome, Option<tokio::sync::mpsc::Receiver<Status>>) {
        let written = write_frames(&self.attached.socket, frames).await;
        let refusal = match written {
            // a socket that is not there is a session that is not running. The
            // item stays queued: a closed session is never silently recreated,
            // and a replacement would not be the session anyone attached.
            Err(WriteFailed::Unreachable) => Some(Outcome::Deferred {
                reason: "inbox_unreachable".to_string(),
            }),
            // the recipient destroys the connection on a bad auth line, which
            // is the ONE negative signal the socket itself carries.
            Err(WriteFailed::Rejected) => Some(Outcome::Refused {
                reason: "auth_rejected".to_string(),
            }),
            Err(WriteFailed::Io) => Some(Outcome::Deferred {
                reason: "inbox_write_failed".to_string(),
            }),
            Ok(()) => None,
        };
        if let Some(refusal) = refusal {
            return (refusal, verdicts);
        }
        let Some(mut verdicts) = verdicts else {
            return (
                Outcome::Unknown {
                    reason: "no_receipt_inbox".to_string(),
                },
                None,
            );
        };
        let outcome = await_verdict(&mut verdicts).await;
        (outcome, Some(verdicts))
    }

    /// the two lines that go down the socket, in order.
    ///
    /// Built as data and rendered by the caller so the frame shape is
    /// assertable without a live session — which is the only way to test that
    /// `session_id` is on the frame at all, and that is the fencing the
    /// recipient enforces for us.
    fn frames(&self, offer: &Offer<'_>, correlation: &str, reply_to: Option<&str>) -> Vec<String> {
        let auth = serde_json::json!({ "type": "auth", "token": self.attached.token });
        let mut message = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": offer.text },
            // the recipient drops a frame whose session_id is not its own, so
            // sending it turns "this pid" into "this session" — the difference
            // between writing to the session someone attached and writing to
            // whatever now holds that pid.
            "session_id": self.attached.session_id,
            "msg_id": correlation,
            // urgent input is offered at the head of the queue; ordinary input
            // waits its turn. Neither interrupts a running tool.
            "priority": if offer.urgent { "now" } else { "next" },
        });
        if let Some(reply_to) = reply_to {
            message["from"] = serde_json::Value::String(reply_to.to_string());
        }
        vec![auth.to_string(), message.to_string()]
    }
}

/// map the recipient's verdict, or its silence, onto an outcome.
async fn await_verdict(verdicts: &mut tokio::sync::mpsc::Receiver<Status>) -> Outcome {
    let Ok(Some(status)) = tokio::time::timeout(VERDICT_WINDOW, verdicts.recv()).await else {
        // silence. The message may be sitting in the session's queue, and it
        // may have been dropped for a reason that carries no receipt. Nothing
        // here knows which, so nothing here may say.
        return Outcome::Unknown {
            reason: "no_acceptance_signal".to_string(),
        };
    };
    verdict_outcome(status)
}

/// A held message, still listening on the registration its offer made.
///
/// Held is not an ending: the recipient may release it, and that release is the
/// ONLY acceptance signal a Claude inbox produces. Without this the release
/// arrived for a waiter that had already been taken out, and a message a human
/// approved stayed `held` on-chain forever.
///
/// It is the OWNER of the registration: dropping it — resolved, timed out, or
/// the lane task going away — takes the waiter back out of the map.
pub struct Follow {
    verdicts: tokio::sync::mpsc::Receiver<Status>,
    receipts: Arc<Receipts>,
    correlation: String,
}

impl Follow {
    /// Wait for the hold to resolve.
    ///
    /// `None` is "nothing more was said": the window passed, or the session
    /// went away. The record stays `Held` — which is what was observed, is not
    /// terminal, and is never replayed automatically. Reporting an acceptance
    /// or a refusal on a timeout would be inventing the one fact this whole
    /// channel exists to avoid inventing.
    ///
    /// [`HOLD_WINDOW`] is a LOCAL resource bound on how long this task lives,
    /// never an authorization: the message's real deadline is `expires_at` on
    /// the agreed clock, which the network enforces and this process cannot
    /// convert into wall-clock seconds.
    pub async fn resolve(mut self) -> Option<Outcome> {
        let Ok(Some(status)) = tokio::time::timeout(HOLD_WINDOW, self.verdicts.recv()).await else {
            return None;
        };
        Some(verdict_outcome(status))
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        self.receipts.forget(&self.correlation);
    }
}

/// the identity of one message, as the back-channel spells it: the chat message
/// id itself, which is what a verdict's `orig_msg_id` carries back.
fn correlation(message_id: &str) -> String {
    message_id.to_string()
}

/// one verdict as an outcome. Shared by the first wait and the hold follow-up,
/// so a `delivered` means the same thing whenever it arrives.
fn verdict_outcome(status: Status) -> Outcome {
    match status {
        // a barrier is holding it. Exposed, not overridden.
        Status::Held => Outcome::Held {
            reason: "provider_hold".to_string(),
        },
        // the only positive signal that exists: a previously held message
        // released to the session. It is the recipient's own statement, which
        // is what makes it an acceptance rather than an inference.
        Status::Delivered => Outcome::Accepted {
            reason: Some("hold_released".to_string()),
        },
        Status::Denied => Outcome::Refused {
            reason: "recipient_denied".to_string(),
        },
        Status::Refused => Outcome::Refused {
            reason: "inbox_refused".to_string(),
        },
        Status::Dropped { reason } => Outcome::Refused { reason },
        Status::Expired => Outcome::Expired {
            reason: "hold_expired".to_string(),
        },
    }
}

/// what the recipient said about one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Held,
    Delivered,
    Denied,
    Refused,
    Dropped { reason: String },
    Expired,
}

impl Status {
    /// read one `peer_message_status` frame. `None` for anything else on the
    /// socket — this inbox exists to hear verdicts and nothing more.
    fn parse(frame: &serde_json::Value) -> Option<(String, Status)> {
        let is_status = frame["type"] == "control" && frame["action"] == "peer_message_status";
        if !is_status {
            return None;
        }
        let correlation = frame["orig_msg_id"].as_str()?.to_string();
        let status = match frame["status"].as_str()? {
            "held" => Status::Held,
            "delivered" => Status::Delivered,
            "denied" => Status::Denied,
            "refused" => Status::Refused,
            "expired" => Status::Expired,
            "dropped" => Status::Dropped {
                reason: drop_reason(frame["drop_reason"].as_str()),
            },
            _ => return None,
        };
        Some((correlation, status))
    }
}

/// normalize the recipient's drop reason to a stable snake_case token. An
/// unrecognized one becomes a single named token rather than being passed
/// through: a `reason` is greppable and countable, not prose from elsewhere.
fn drop_reason(raw: Option<&str>) -> String {
    match raw {
        Some("queue-full") => "recipient_queue_full".to_string(),
        Some("rate-limit") => "recipient_rate_limited".to_string(),
        Some("duplicate") => "recipient_duplicate".to_string(),
        Some("relay-loop") => "recipient_relay_loop".to_string(),
        Some(_) | None => "recipient_dropped".to_string(),
    }
}

/// why a write did not land.
enum WriteFailed {
    /// the socket is not there, or nothing is listening: the session is gone.
    Unreachable,
    /// the recipient closed the connection on us — a rejected auth line.
    Rejected,
    /// something else went wrong locally.
    Io,
}

/// connect, write every line, and find out whether the recipient hung up.
///
/// The half-close is what makes the auth check observable. After the frames go
/// out we shut the write side down and read: a recipient that accepted the
/// connection answers EOF (it never writes), and one that rejected the auth
/// line has already destroyed the socket. Those are distinguishable, and they
/// are the only two things this connection can tell us.
async fn write_frames(socket: &Path, frames: &[String]) -> Result<(), WriteFailed> {
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|_| WriteFailed::Unreachable)?;
    for frame in frames {
        let line = format!("{frame}\n");
        if stream.write_all(line.as_bytes()).await.is_err() {
            // a write that fails mid-sequence is the recipient having closed
            // on the line before it — an unauthenticated drop.
            return Err(WriteFailed::Rejected);
        }
    }
    stream.flush().await.map_err(|_| WriteFailed::Io)?;
    Ok(())
}

/// this daemon's own address, and the verdicts that arrive on it.
///
/// The recipient only answers a sender it can vet: a `uds:` address inside the
/// same sockets namespace, owned by a permitted uid, whose peer pid matches.
/// So the back-channel costs us an address of our own — bound at our own pid,
/// in the directory the sessions already share.
///
/// It listens and nothing else. No message is ever accepted here, no session
/// is impersonated, and nothing is published into the session registry: this
/// process is not a Claude session and does not present itself as one.
pub struct Receipts {
    address: PathBuf,
    /// one lane per outstanding delivery, NOT a one-shot: a hold is followed to
    /// its resolution, so `held` and the `delivered` that releases it are two
    /// verdicts about one message and both have to reach the same waiter. The
    /// registration is made once, before the write, and taken out once, by the
    /// [`Follow`] that owns it — there is no window in between for a release to
    /// fall through.
    waiting: std::sync::Mutex<std::collections::HashMap<String, Verdicts>>,
    /// the pids this daemon has bound a session to.
    ///
    /// A unix socket in a shared directory is reachable by anything running as
    /// this user, and a verdict is a statement this daemon RECORDS — so
    /// "anyone local may say a message was refused" is not an acceptable
    /// authority. A connection's peer credentials are checked against the
    /// sessions actually bound, and everything else is dropped. The correlation
    /// ids are predictable (they are the sender's own message ids), so this is
    /// the only thing standing between a local process and a forged receipt.
    trusted_pids: std::sync::Mutex<std::collections::HashSet<u32>>,
}

impl Receipts {
    /// bind the receipt address and serve it until the process ends.
    ///
    /// `Err` is not fatal to anything: the caller runs without a back-channel,
    /// and every Claude delivery then settles `DeliveryUnknown`. A pre-existing
    /// socket file is left ALONE rather than unlinked — it may belong to a live
    /// session, and reclaiming another process's address to gain a receipt
    /// channel is not a trade this daemon makes.
    pub async fn bind(sockets_dir: &Path) -> Result<Arc<Self>, String> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        // the directory and the socket are the user's own, as the interface
        // above states: 0700 around 0600. A directory already there keeps its
        // mode — Claude Code made it, to the same rule.
        let mut directory = std::fs::DirBuilder::new();
        directory.recursive(true).mode(0o700);
        directory
            .create(sockets_dir)
            .map_err(|error| format!("create sockets dir: {error}"))?;
        let address = sockets_dir.join(format!("{}.sock", std::process::id()));
        let listener = tokio::net::UnixListener::bind(&address)
            .map_err(|error| format!("bind receipt address: {error}"))?;
        std::fs::set_permissions(&address, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("guard receipt address: {error}"))?;
        let receipts = Arc::new(Self {
            address,
            waiting: std::sync::Mutex::new(std::collections::HashMap::new()),
            trusted_pids: std::sync::Mutex::new(std::collections::HashSet::new()),
        });
        let serving = receipts.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let receipts = serving.clone();
                tokio::spawn(async move { receipts.serve(stream).await });
            }
        });
        Ok(receipts)
    }

    /// the address to put in a `from` field.
    pub fn address(&self) -> &Path {
        &self.address
    }

    /// trust verdicts from one bound session's process.
    ///
    /// Called when a binding resolves, so the set is exactly the sessions this
    /// daemon has been told to talk to — never "every Claude session this user
    /// happens to run", which would be the scanning this design refuses
    /// elsewhere reintroduced through the back door.
    pub fn trust(&self, pid: u32) {
        self.trusted_pids
            .lock()
            .expect("receipt trust lock poisoned")
            .insert(pid);
    }

    /// whether a connecting process is one of them.
    fn trusts(&self, pid: u32) -> bool {
        self.trusted_pids
            .lock()
            .expect("receipt trust lock poisoned")
            .contains(&pid)
    }

    /// register interest in one message's verdicts, before it is sent.
    pub fn expect(&self, correlation: &str) -> tokio::sync::mpsc::Receiver<Status> {
        let (tx, rx) = tokio::sync::mpsc::channel(VERDICTS_PER_MESSAGE);
        self.waiting
            .lock()
            .expect("receipt waiters lock poisoned")
            .insert(correlation.to_string(), tx);
        rx
    }

    /// stop waiting for one message's verdict.
    ///
    /// The counterpart to [`Receipts::expect`], for every path that registers a
    /// waiter and then never gets a verdict: a refused connection, a session
    /// that was not there, a window that passed. Without it each of those
    /// leaves a sender behind, and nothing else ever drains this map.
    fn forget(&self, correlation: &str) {
        self.waiting
            .lock()
            .expect("receipt waiters lock poisoned")
            .remove(correlation);
    }

    /// hand a verdict to whoever is waiting for it.
    ///
    /// The registration STAYS after a send: a `held` is followed by the
    /// `delivered` that releases it, and removing the lane on the first would
    /// drop the second — which is the acceptance signal this adapter has and no
    /// other. [`Follow`] removes it when it stops listening.
    ///
    /// A verdict nobody awaits is DROPPED with a debug line rather than
    /// queued: it is a late transition on a message whose delivery attempt has
    /// already settled, and buffering it would grow a map nothing drains.
    fn deliver(&self, correlation: &str, status: Status) {
        let waiter = self
            .waiting
            .lock()
            .expect("receipt waiters lock poisoned")
            .get(correlation)
            .cloned();
        let Some(waiter) = waiter else {
            tracing::debug!(
                target: "ducktape::collab",
                reason = "unawaited_verdict",
                status = status_token(&status),
                "a provider verdict arrived for no outstanding delivery"
            );
            return;
        };
        // `try_send`, because this runs on the connection's read loop and a
        // waiter that has stopped reading must not stall the peer's other
        // verdicts. A full lane means this message already has more verdicts
        // outstanding than a message can honestly have.
        if waiter.try_send(status).is_err() {
            tracing::debug!(
                target: "ducktape::collab",
                reason = "verdict_lane_full",
                "dropped a provider verdict: its waiter is not reading"
            );
        }
    }

    /// read newline-delimited verdicts off one connection, from a peer this
    /// daemon actually bound.
    ///
    /// The peer credentials come from the kernel, not from the frame: a `from`
    /// or a pid written INTO a verdict would be the peer vouching for itself.
    async fn serve(&self, stream: tokio::net::UnixStream) {
        let peer = stream.peer_cred().ok().and_then(|cred| cred.pid());
        let Some(pid) = peer.and_then(|pid| u32::try_from(pid).ok()) else {
            tracing::debug!(
                target: "ducktape::collab",
                reason = "receipt_peer_unidentified",
                "dropped a receipt connection whose peer could not be identified"
            );
            return;
        };
        if !self.trusts(pid) {
            tracing::warn!(
                target: "ducktape::collab",
                reason = "receipt_peer_untrusted",
                "dropped a receipt connection from a process no binding names"
            );
            return;
        }
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some((correlation, status)) = Status::parse(&frame) {
                self.deliver(&correlation, status);
            }
        }
    }
}

/// the stable token for a verdict, for logs.
fn status_token(status: &Status) -> &'static str {
    match status {
        Status::Held => "held",
        Status::Delivered => "delivered",
        Status::Denied => "denied",
        Status::Refused => "refused",
        Status::Dropped { .. } => "dropped",
        Status::Expired => "expired",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attached(session_id: &str, socket: &Path) -> Attached {
        Attached {
            pid: 4242,
            socket: socket.to_path_buf(),
            token: "s3cr3t-peer-token".to_string(),
            session_id: session_id.to_string(),
        }
    }

    fn an_offer(text: &str) -> Offer<'_> {
        Offer {
            text,
            message_id: "m-7",
            urgent: false,
        }
    }

    /// The key file name is DERIVED from the socket path, so a session that
    /// moved its socket aside cannot have the key for its old one picked up.
    /// Pinned against the shape the installed build publishes.
    #[test]
    fn the_key_file_is_named_for_the_pid_and_its_socket() {
        let name = key_file_name(2308404, "/run/user/1000/cc-socks/2308404.sock");
        assert!(name.starts_with("2308404."), "{name}");
        assert!(name.ends_with(".key"), "{name}");
        // sha256 hex is 64 chars: "<pid>." + 64 + ".key"
        assert_eq!(name.len(), "2308404.".len() + 64 + ".key".len(), "{name}");
        // a different socket path for the same pid is a different key file.
        assert_ne!(
            name,
            key_file_name(2308404, "/run/user/1000/cc-socks/2308404-aabbccdd.sock")
        );
    }

    /// The frame the recipient actually enforces our binding with. Without
    /// `session_id` on it, a reused pid would be written into by accident.
    #[test]
    fn the_user_frame_names_the_bound_session_and_the_message() {
        let inbox = ClaudeInbox::new(attached("sess-abc", Path::new("/tmp/x.sock")), None);
        let offer = an_offer("hello");
        let frames = inbox.frames(
            &offer,
            &correlation(offer.message_id),
            Some("uds:/tmp/me.sock"),
        );
        assert_eq!(frames.len(), 2, "auth first, then the message");

        let auth: serde_json::Value = serde_json::from_str(&frames[0]).expect("auth is json");
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["token"], "s3cr3t-peer-token");

        let message: serde_json::Value = serde_json::from_str(&frames[1]).expect("message is json");
        assert_eq!(message["type"], "user");
        assert_eq!(message["message"]["role"], "user");
        assert_eq!(message["message"]["content"], "hello");
        assert_eq!(
            message["session_id"], "sess-abc",
            "the recipient fences on this; without it we write to a pid, not a session"
        );
        assert_eq!(
            message["msg_id"], "m-7",
            "the verdict's orig_msg_id is this, so it is the chat message id"
        );
        assert_eq!(message["from"], "uds:/tmp/me.sock");
        assert_eq!(message["priority"], "next");
    }

    #[test]
    fn an_urgent_offer_asks_for_the_head_of_the_queue() {
        let inbox = ClaudeInbox::new(attached("sess-abc", Path::new("/tmp/x.sock")), None);
        let mut offer = an_offer("wake up");
        offer.urgent = true;
        let frames = inbox.frames(&offer, "m-7", None);
        let message: serde_json::Value = serde_json::from_str(&frames[1]).expect("json");
        assert_eq!(message["priority"], "now");
        assert!(
            message.get("from").is_none(),
            "without a receipt inbox there is no address to answer"
        );
    }

    /// The verdict vocabulary, mapped. `delivered` is the ONLY thing that may
    /// become an acceptance, because it is the only positive statement the
    /// recipient ever makes.
    #[tokio::test]
    async fn each_verdict_maps_to_the_state_the_recipient_reported() {
        let cases = [
            (Status::Held, "held"),
            (Status::Delivered, "accepted"),
            (Status::Denied, "refused"),
            (Status::Refused, "refused"),
            (Status::Expired, "expired"),
            (
                Status::Dropped {
                    reason: "recipient_queue_full".to_string(),
                },
                "refused",
            ),
        ];
        for (status, expected) in cases {
            let (tx, mut rx) = tokio::sync::mpsc::channel(VERDICTS_PER_MESSAGE);
            tx.send(status.clone()).await.expect("the waiter is live");
            let got = match await_verdict(&mut rx).await {
                Outcome::Accepted { .. } => "accepted",
                Outcome::Held { .. } => "held",
                Outcome::Refused { .. } => "refused",
                Outcome::Expired { .. } => "expired",
                Outcome::Unknown { .. } => "unknown",
                Outcome::Deferred { .. } => "not_attached",
            };
            assert_eq!(got, expected, "{status:?} must map to {expected}");
        }
    }

    /// THE rule. A message that routes fine is silent, so silence must never
    /// become an acceptance — the whole difference between reporting a
    /// delivery and fabricating one.
    #[tokio::test(start_paused = true)]
    async fn silence_is_unknown_and_never_an_acceptance() {
        let (_tx, mut rx) = tokio::sync::mpsc::channel(VERDICTS_PER_MESSAGE);
        let outcome = await_verdict(&mut rx).await;
        let Outcome::Unknown { reason } = outcome else {
            panic!("silence must be unknown, got {outcome:?}");
        };
        assert_eq!(reason, "no_acceptance_signal");
    }

    /// A dropped sender is the receipt inbox going away mid-flight. Same rule:
    /// we learned nothing, so we say nothing.
    #[tokio::test]
    async fn a_lost_receipt_channel_is_unknown_too() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Status>(VERDICTS_PER_MESSAGE);
        drop(tx);
        assert!(matches!(
            await_verdict(&mut rx).await,
            Outcome::Unknown { .. }
        ));
    }

    #[test]
    fn a_status_frame_is_read_and_anything_else_is_ignored() {
        let held = serde_json::json!({
            "type": "control",
            "action": "peer_message_status",
            "status": "held",
            "orig_msg_id": "2.1",
        });
        assert_eq!(
            Status::parse(&held),
            Some(("2.1".to_string(), Status::Held))
        );

        let dropped = serde_json::json!({
            "type": "control",
            "action": "peer_message_status",
            "status": "dropped",
            "drop_reason": "queue-full",
            "orig_msg_id": "2.1",
        });
        assert_eq!(
            Status::parse(&dropped),
            Some((
                "2.1".to_string(),
                Status::Dropped {
                    reason: "recipient_queue_full".to_string()
                }
            ))
        );

        // this inbox hears verdicts. It is not a message inbox, and a `user`
        // frame arriving on it is not one to route anywhere.
        for other in [
            serde_json::json!({"type": "user", "message": {"role": "user", "content": "hi"}}),
            serde_json::json!({"type": "control", "action": "rename", "name": "x"}),
            serde_json::json!({"type": "control", "action": "peer_message_status", "status": "invented", "orig_msg_id": "2.1"}),
        ] {
            assert_eq!(Status::parse(&other), None, "must ignore {other}");
        }
    }

    #[test]
    fn an_unknown_drop_reason_becomes_one_named_token_not_prose() {
        assert_eq!(drop_reason(Some("queue-full")), "recipient_queue_full");
        assert_eq!(
            drop_reason(Some("something the recipient invented")),
            "recipient_dropped"
        );
        assert_eq!(drop_reason(None), "recipient_dropped");
    }

    /// The token must not be reachable through the ordinary escape hatch.
    #[test]
    fn debugging_an_attachment_does_not_print_its_token_or_socket() {
        let rendered = format!(
            "{:?}",
            attached("sess-abc", Path::new("/run/user/1000/cc-socks/4242.sock"))
        );
        assert!(
            rendered.contains("4242"),
            "the pid is diagnosable: {rendered}"
        );
        assert!(!rendered.contains("s3cr3t"), "token leaked: {rendered}");
        assert!(!rendered.contains("cc-socks"), "socket leaked: {rendered}");
    }

    async fn registry_with(entries: &[(&str, u32, Option<&str>)], keys: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ducktape-claude-registry-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("registry dir");
        for (session_id, pid, socket) in entries {
            let entry = match socket {
                Some(socket) => serde_json::json!({
                    "pid": pid, "sessionId": session_id, "messagingSocketPath": socket,
                }),
                None => serde_json::json!({ "pid": pid, "sessionId": session_id }),
            };
            std::fs::write(dir.join(format!("{pid}.json")), entry.to_string()).expect("entry");
            if keys && let Some(socket) = socket {
                std::fs::write(
                    dir.join(key_file_name(*pid, socket)),
                    serde_json::json!({ "peerToken": "tok" }).to_string(),
                )
                .expect("key");
            }
        }
        dir
    }

    #[tokio::test]
    async fn resolving_takes_the_one_named_session_and_leaves_the_rest_alone() {
        // three sessions belong to this operating-system user. A binding named
        // ONE of them; the other two are not ours to write into.
        let dir = registry_with(
            &[
                ("sess-a", 101, Some("/tmp/a.sock")),
                ("sess-b", 102, Some("/tmp/b.sock")),
                ("sess-c", 103, Some("/tmp/c.sock")),
            ],
            true,
        )
        .await;
        let attached = resolve(&dir, "sess-b").await.expect("resolves");
        assert_eq!(attached.pid, 102);
        assert_eq!(attached.session_id, "sess-b");
        assert_eq!(attached.socket, Path::new("/tmp/b.sock"));

        assert_eq!(
            resolve(&dir, "sess-missing").await.err(),
            Some(Unreachable::NoSuchSession)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_ambiguous_session_id_is_refused_rather_than_guessed() {
        let dir = registry_with(
            &[
                ("sess-dup", 201, Some("/tmp/one.sock")),
                ("sess-dup", 202, Some("/tmp/two.sock")),
            ],
            true,
        )
        .await;
        assert_eq!(
            resolve(&dir, "sess-dup").await.err(),
            Some(Unreachable::Ambiguous),
            "picking the first match would write into an arbitrary one of two sessions"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_session_with_no_inbox_or_no_key_is_unreachable_not_attached() {
        let no_inbox = registry_with(&[("sess-quiet", 301, None)], false).await;
        assert_eq!(
            resolve(&no_inbox, "sess-quiet").await.err(),
            Some(Unreachable::NoInbox)
        );
        let _ = std::fs::remove_dir_all(&no_inbox);

        let no_key = registry_with(&[("sess-nokey", 302, Some("/tmp/n.sock"))], false).await;
        assert_eq!(
            resolve(&no_key, "sess-nokey").await.err(),
            Some(Unreachable::NoKey)
        );
        let _ = std::fs::remove_dir_all(&no_key);
    }

    /// A session that is not running is not a session to replace.
    #[tokio::test]
    async fn a_missing_socket_leaves_the_message_queued() {
        let inbox = ClaudeInbox::new(
            attached("sess-abc", Path::new("/tmp/ducktape-no-such-inbox.sock")),
            None,
        );
        let (outcome, follow) = inbox.offer(&an_offer("hello")).await;
        let Outcome::Deferred { reason } = outcome else {
            panic!("a closed session must not settle, got {outcome:?}");
        };
        assert_eq!(reason, "inbox_unreachable");
        assert!(
            follow.is_none(),
            "nothing was written, so there is no hold to follow"
        );
    }

    /// The back-channel, end to end over a real unix socket: a verdict written
    /// by a peer reaches the delivery that was waiting for it.
    #[tokio::test]
    async fn a_verdict_written_to_the_receipt_address_reaches_its_waiter() {
        let dir = std::env::temp_dir().join(format!("ducktape-receipts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let receipts = Receipts::bind(&dir).await.expect("binds");
        // the user's own, as the interface states: 0700 around 0600
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| std::fs::metadata(path).expect("exists").permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700, "the sockets dir is the user's own");
            assert_eq!(mode(receipts.address()), 0o600, "the receipt address is the user's own");
        }
        // the peer below is this process, and a verdict is only read from a
        // process a BINDING named. Without this the connection is dropped on
        // the credential check and the waiter below never resolves at all.
        receipts.trust(std::process::id());
        let mut waiter = receipts.expect("2.1");

        let mut peer = tokio::net::UnixStream::connect(receipts.address())
            .await
            .expect("a peer can reach the receipt address");
        let frame = serde_json::json!({
            "type": "control",
            "action": "peer_message_status",
            "status": "held",
            "orig_msg_id": "2.1",
        });
        peer.write_all(format!("{frame}\n").as_bytes())
            .await
            .expect("writes");
        peer.flush().await.expect("flushes");

        // synchronized on the verdict arriving, not on a sleep.
        assert_eq!(
            waiter.recv().await.expect("a verdict arrives"),
            Status::Held
        );

        // and the SECOND one, on the same registration: a held message is
        // released later, and that release is this adapter's only acceptance
        // signal. A registration consumed by the first verdict would drop it.
        let released = serde_json::json!({
            "type": "control",
            "action": "peer_message_status",
            "status": "delivered",
            "orig_msg_id": "2.1",
        });
        peer.write_all(format!("{released}\n").as_bytes())
            .await
            .expect("writes");
        peer.flush().await.expect("flushes");
        assert_eq!(
            waiter.recv().await.expect("the release arrives"),
            Status::Delivered,
            "the hold's resolution must reach the same waiter as the hold"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A verdict is only read from a process a BINDING named. One from anywhere
    /// else is dropped — and dropping it must be an EVENT, not a silence: a
    /// waiter left in the map forever is how the connection check turned into a
    /// test that hung instead of failing.
    #[tokio::test]
    async fn a_verdict_from_a_process_no_binding_names_is_dropped() {
        let receipts = Receipts {
            address: PathBuf::from("/tmp/ducktape-unused.sock"),
            waiting: std::sync::Mutex::new(std::collections::HashMap::new()),
            trusted_pids: std::sync::Mutex::new(std::collections::HashSet::new()),
        };
        let waiter = receipts.expect("2.1");

        // both ends are this process, and no binding has named it.
        let (mut peer, ours) = tokio::net::UnixStream::pair().expect("a socket pair");
        let frame = serde_json::json!({
            "type": "control",
            "action": "peer_message_status",
            "status": "delivered",
            "orig_msg_id": "2.1",
        });
        peer.write_all(format!("{frame}\n").as_bytes())
            .await
            .expect("writes");
        peer.flush().await.expect("flushes");

        // `serve` returns once it has decided, so this is the decision itself
        // rather than a wait for one.
        receipts.serve(ours).await;
        assert!(
            receipts.waiting.lock().expect("lock").contains_key("2.1"),
            "an untrusted verdict must not resolve a waiter"
        );

        // and the offer path takes its own waiter back, so a verdict that never
        // comes does not leave a sender behind for the life of the process.
        receipts.forget("2.1");
        assert!(receipts.waiting.lock().expect("lock").is_empty());
        drop(waiter);
    }
}
