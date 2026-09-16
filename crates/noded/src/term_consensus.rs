//! Shared-terminal commands arrive as ordered, authenticated chat messages.
//! The node creates a public `term-<session_id>` channel under its own signer.
//! Losing that create race is safe only after confirming the existing owner
//! against that same key or its current identity account.
//!
//! The projector admits only commands from the channel owner. Account owners
//! use canonical account authorship; historical key owners require the exact
//! authenticated current content origin. Another key joining the account never inherits a
//! historical key's terminal grant. Module/system ownership cannot start a
//! projector. Tombstones never execute.
//!
//! The node-local terminal lane and the committed-message polling schedule
//! remain separate: this module supplies the consensus-ordered command source.

use std::time::Duration;

use crate::{NodeCommand, NodeHandle};

/// how often the projector polls committed chat for new commands. A shared
/// terminal is human-driven, so ~200 ms adds no perceptible latency while
/// keeping the poll cheap (at most [`agent_service::MAX_TERM_SESSIONS`] live
/// sessions, one bounded query each).
/// `ponytail:` fixed interval poll; a block-commit watch (StreamHub already
/// has `subscribe_blocks`) would cut idle polls and trim the latency floor if a
/// future workload proves it matters.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

pub use ducktape_terminal::consensus::{command_blocks, command_text, session_channel};
use ducktape_terminal::consensus::{channel_owner, project_message};

/// what [`ensure_channel`] learned about the session's command channel.
///
/// A plain `Result<(), String>` used to treat "already exists" as success
/// outright — which is exactly the hole #1746 closes: "already exists" means
/// somebody else's `CreateChannel` won a race for this id, and that somebody
/// might not be this node. `Squatted` and `Failed` get different treatment
/// from [`crate::term::create_local`]: a squat is fatal to a `Shared` session
/// (closed, never silently driven by whoever squatted it), while a transient
/// failure degrades to the node-local ws command lane, exactly as before.
pub(crate) enum EnsureChannelOutcome {
    /// the channel exists AND this node's own account is its `owner` — safe
    /// for [`spawn_projector`] to trust.
    Ready,
    /// the channel exists under a DIFFERENT owner. Carries the stable reason
    /// [`confirm_ownership`] resolved it to.
    Squatted(&'static str),
    /// neither of the above — an actor/transport failure, never an
    /// authorization outcome. Carries the raw (free-form) detail.
    Failed(String),
}

/// ensure the session's command channel exists AND this node owns it
/// (idempotent). Submits a chat `CreateChannel` under the node's own key with
/// an OPEN post policy — the log is public; [`project_message`] is what
/// decides which post drives the pty. On `bin/node` the origin bytes are
/// ignored and the node key signs; on `bin/noded` they become the external
/// author — either way a `User` origin authoring a non-colon id, which chat
/// accepts, and which becomes the `Channel.owner` the projector gates on.
///
/// "already exists" used to be treated as success outright: a member watching
/// this session's chunk fanout learns its id and can `CreateChannel` the same
/// `term-<id>` first, and if that commit lands ahead of this node's own, this
/// node's create loses the race — but the caller has no way to tell "I already
/// had it" from "someone else already has it" from the error string alone. So
/// a race loss is followed by [`confirm_ownership`], which asks the one
/// question that matters: is the channel that already exists OURS.
pub(crate) async fn ensure_channel(handle: &NodeHandle, channel: &str) -> EnsureChannelOutcome {
    let (reply, rx) = futures::channel::oneshot::channel();
    let payload = chat::encode_msg(&chat::ChatMsg::CreateChannel {
        channel_id: channel.to_string(),
        name: channel.to_string(),
        post_policy: chat::PostPolicy::Open,
    });
    let sent = handle
        .send(NodeCommand::Submit {
            required_blob: None,
            target: chat::DEFAULT_CHAT_TARGET.to_string(),
            payload,
            origin: crate::DEFAULT_ORIGIN.as_bytes().to_vec(),
            reply,
        })
        .await;
    if sent.is_err() {
        return EnsureChannelOutcome::Failed("actor gone".to_string());
    }
    match rx.await {
        Err(_) => EnsureChannelOutcome::Failed("reply dropped".to_string()),
        Ok(Ok(_)) => EnsureChannelOutcome::Ready,
        // a concurrent create raced us to the same id: the channel is there,
        // but "there" is not "ours" — confirm it before trusting it.
        Ok(Err(reason)) if reason.contains("already exists") => {
            confirm_ownership(handle, channel).await
        }
        Ok(Err(reason)) => EnsureChannelOutcome::Failed(reason),
    }
}

/// This node's actual signing key, read from local status.
///
/// Mirrors the origin resolution [`ensure_channel`]'s doc describes: `bin/node`
/// signs a `Submit` with its own node key regardless of the `origin` field, and
/// that key is exactly `NodeStatus.public_key` (`bin/node/src/boot/mesh.rs`
/// derives both from the same signer) — published once at boot, so reading it
/// back here costs nothing that reaching consensus would. A daemon with no mesh
/// identity (the embedded local daemon, `bin/noded`) publishes an empty
/// `public_key`, and on that path the origin field IS the author: the literal
/// [`crate::DEFAULT_ORIGIN`] bytes `ensure_channel` submits.
fn self_key(handle: &NodeHandle) -> Vec<u8> {
    let public_key = handle.status_cell().current().public_key;
    match crate::term::decode_node_key(&public_key) {
        Some(bytes) => bytes.to_vec(),
        None => crate::DEFAULT_ORIGIN.as_bytes().to_vec(),
    }
}

/// prove this node's own account owns `channel` after losing the create race
/// for it — a READ, not a write: `query_channel` is the same `ChatQuery::Channel`
/// point read [`projector_loop`] already trusts, so this asks it the one
/// question that matters BEFORE the projector ever spawns, instead of after.
/// No new chat op and no new query shape — `Channel.owner` was always the
/// right anchor, the missing piece was comparing it against something.
async fn confirm_ownership(handle: &NodeHandle, channel: &str) -> EnsureChannelOutcome {
    let record = match query_channel(handle, channel).await {
        Ok(record) => record,
        Err(reason) => return EnsureChannelOutcome::Failed(reason),
    };
    let owner = match channel_owner(record) {
        Ok(owner) => owner,
        Err(_) => return EnsureChannelOutcome::Squatted("channel_owned_by_another_account"),
    };
    let key = self_key(handle);
    let owns_channel = match owner {
        chat::Party::Key(owner) => owner == key,
        chat::Party::Account(owner) => match account_of_key(handle, key).await {
            Ok(account) => account == Some(owner),
            Err(reason) => return EnsureChannelOutcome::Failed(reason),
        },
        chat::Party::Module(_) | chat::Party::System => false,
    };
    if owns_channel {
        return EnsureChannelOutcome::Ready;
    }
    EnsureChannelOutcome::Squatted("channel_owned_by_another_account")
}

/// the account `key` belongs to, read from committed identity state over the
/// command lane. shared with the huddle gates (`crate::call`, the node-proof
/// mint), which admit only a key that holds an account.
pub(crate) async fn account_of_key(
    handle: &NodeHandle,
    key: Vec<u8>,
) -> Result<Option<u64>, String> {
    let (reply, rx) = futures::channel::oneshot::channel();
    handle
        .send(NodeCommand::Query {
            target: "identity".into(),
            req: identity::encode_query(&identity::IdentityQuery::OfKey { key }),
            reply,
        })
        .await
        .map_err(|_| "actor gone".to_string())?;
    let bytes = rx.await.map_err(|_| "reply dropped".to_string())??;
    let identity::IdentityReply::Account(account) = identity::decode_reply(&bytes)? else {
        return Err("unexpected identity reply".into());
    };
    Ok(account.map(|account| account.number))
}

/// query committed chat for the session channel's messages with `seq >= from_seq`,
/// ascending. Rides the ordinary command lane (`NodeCommand::Query`), so it
/// reads CANONICAL committed state and can never re-enter the consensus loop —
/// the projector task is not that loop.
async fn query_messages(
    handle: &NodeHandle,
    channel: &str,
    from_seq: u64,
) -> Result<Vec<chat::MessageView>, String> {
    let (reply, rx) = futures::channel::oneshot::channel();
    let req = chat::encode_query(&chat::ChatQuery::MessagesRange {
        channel_id: channel.to_string(),
        from_seq,
        limit: chat::MAX_QUERY_LIMIT,
    });
    handle
        .send(NodeCommand::Query {
            target: chat::DEFAULT_CHAT_TARGET.to_string(),
            req,
            reply,
        })
        .await
        .map_err(|_| "actor gone".to_string())?;
    let bytes = rx.await.map_err(|_| "reply dropped".to_string())??;
    match chat::decode_reply(&bytes)? {
        chat::ChatReply::Messages(views) => Ok(views),
        _ => Err("unexpected chat reply".to_string()),
    }
}

/// query committed chat for the session channel's record — the projector reads
/// it once, for `Channel.owner`. Rides the same command lane as
/// [`query_messages`].
pub(crate) async fn query_channel(
    handle: &NodeHandle,
    channel: &str,
) -> Result<Option<chat::Channel>, String> {
    let (reply, rx) = futures::channel::oneshot::channel();
    let req = chat::encode_query(&chat::ChatQuery::Channel {
        channel_id: channel.to_string(),
    });
    handle
        .send(NodeCommand::Query {
            target: chat::DEFAULT_CHAT_TARGET.to_string(),
            req,
            reply,
        })
        .await
        .map_err(|_| "actor gone".to_string())?;
    let bytes = rx.await.map_err(|_| "reply dropped".to_string())??;
    match chat::decode_reply(&bytes)? {
        chat::ChatReply::Channel(channel) => Ok(channel),
        _ => Err("unexpected chat reply".to_string()),
    }
}

/// spawn the off-loop projector for a freshly created `Shared` session. Wired
/// once, from `create_session`, so it covers both binaries.
pub(crate) fn spawn_projector(handle: NodeHandle, session_id: String) {
    let channel = session_channel(&session_id);
    tokio::spawn(projector_loop(handle, session_id, channel));
}

/// the projector: poll committed chat for the session's channel and drive each
/// new message into the pty, in sequence, with its verified author. The single
/// per-session command consumer (`spawn_command_consumer` in `term.rs`) is what
/// actually feeds the pty and stamps the shared command log; this only enqueues,
/// so consensus order becomes pty order.
///
/// Precedence vs PR1: `enqueue_command` also backs the node-local ws
/// `TermCommand` path, which stays for single-node use. Both feed the SAME FIFO
/// lane, so they interleave in arrival order; for a multi-member shared session
/// the CONSENSUS path is authoritative and a client should drive exactly one
/// source (mutual exclusion is a possible follow-up, not required here).
///
/// Lifecycle: the loop exits the moment the session leaves the manager (EOF,
/// explicit close, or the wall-clock reaper), so it never outlives its pty and
/// never leaks — the same drop-driven teardown the pump and consumer take.
///
/// The owner is resolved ONCE, before the first poll, and the projector refuses
/// to start without one — fail closed. Once resolved it cannot change: chat has
/// no op that rewrites `Channel.owner` (rename and archive touch other fields),
/// so re-reading it per tick would buy nothing and cost a query every 200 ms.
async fn projector_loop(handle: NodeHandle, session_id: String, channel: String) {
    let resolved = query_channel(&handle, &channel)
        .await
        .map_err(|_| "channel_unreadable")
        .and_then(channel_owner);
    let owner = match resolved {
        Ok(owner) => owner,
        // once per session, and the session keeps working on the node-local ws
        // lane — only the consensus lane is refused. No id, no author bytes.
        Err(reason) => {
            tracing::warn!(target: "ducktape::term", reason, "term_consensus_projector_refused");
            return;
        }
    };
    tracing::info!(target: "ducktape::term", session = %session_id, "term_consensus_projector_started");
    // the per-session cursor: the highest chat seq already projected. Starts at
    // 0 — the channel is minted empty at session create, so seq 1 is the first
    // command. A pty session is ephemeral and node-local (it never survives a
    // restart), so there is no durable cursor to restore.
    let mut cursor = 0u64;
    loop {
        // stop as soon as the pty is gone. A session has a `mode` exactly while
        // it is in the bridge's map, so `None` means the entry left it (EOF,
        // close, reaper, or the agent service detaching) and nothing more can be
        // driven.
        match handle.terminals() {
            Some(terminals) if terminals.mode(&session_id).is_some() => {}
            _ => break,
        }
        match query_messages(&handle, &channel, cursor + 1).await {
            Ok(views) => {
                for view in &views {
                    cursor = view.seq;
                    let projected = match project_message(view, &owner) {
                        Ok(projected) => projected,
                        // per-post, so `debug` — a member who spams the open
                        // channel must not evict the log ring. The reason is the
                        // whole diagnosis; the author and the text stay out.
                        Err(reason) => {
                            tracing::debug!(target: "ducktape::term", reason, "term_command_refused");
                            continue;
                        }
                    };
                    // re-fetch the manager each command: it may have gone away
                    // between the query and here. enqueue_command logs the per-
                    // command debug (with seq, never the text) and no-ops+warns
                    // on an unknown/non-shared session, so this stays a no-op if
                    // the session just ended.
                    if let Some(terminals) = handle.terminals() {
                        terminals.enqueue_command(&session_id, projected.origin, projected.text);
                    }
                }
            }
            // transient (actor busy/gone mid-shutdown): retry next tick. debug,
            // not warn — a persistent failure must not bomb the log ring.
            Err(reason) => {
                tracing::debug!(target: "ducktape::term", session = %session_id, reason = %reason, "term_consensus_poll_failed");
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    tracing::info!(target: "ducktape::term", session = %session_id, "term_consensus_projector_stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat::Party;
    use futures::StreamExt as _;

    fn committed_block() -> crate::BlockSummary {
        crate::BlockSummary {
            height: 1,
            root_hash: "ab".repeat(32),
        }
    }

    /// a stand-in chat actor for [`ensure_channel`]/[`confirm_ownership`]:
    /// answers `CreateChannel` with `create_reply` (the first-create outcome)
    /// and `ChatQuery::Channel` with a record owned by `existing_owner` —
    /// [`confirm_ownership`]'s READ, reached only after a create loss. Every
    /// other op panics — these are the only two `ensure_channel`'s path ever
    /// issues.
    fn spawn_chat_actor(
        mut rx: futures::channel::mpsc::Receiver<NodeCommand>,
        create_reply: Result<(), &'static str>,
        existing_owner: Option<Vec<u8>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(cmd) = rx.next().await {
                match cmd {
                    NodeCommand::Submit { payload, reply, .. } => {
                        match chat::decode_msg(&payload).expect("a chat op") {
                            chat::ChatMsg::CreateChannel { .. } => {
                                let _ = reply.send(
                                    create_reply
                                        .map(|()| committed_block())
                                        .map_err(|reason| reason.to_string()),
                                );
                            }
                            other => panic!("ensure_channel submitted an unexpected op: {other:?}"),
                        }
                    }
                    NodeCommand::Query { req, reply, .. } => {
                        let chat::ChatQuery::Channel { channel_id } =
                            chat::decode_query(&req).expect("a chat query")
                        else {
                            panic!("confirm_ownership queried something other than Channel");
                        };
                        let record = existing_owner.clone().map(|owner| chat::Channel {
                            id: channel_id.clone(),
                            name: channel_id,
                            created_at: 0,
                            head_seq: 0,
                            post_policy: chat::PostPolicy::Open,
                            hooks: Vec::new(),
                            pinned: Vec::new(),
                            huddle: Vec::new(),
                            voice: false,
                            owner: chat::Party::Key(owner),
                            revision: 1,
                            archived: false,
                        });
                        let _ =
                            reply.send(Ok(chat::encode_reply(&chat::ChatReply::Channel(record))));
                    }
                    NodeCommand::SubmitFrame { .. } => {
                        panic!("ensure_channel used SubmitFrame unexpectedly")
                    }
                    NodeCommand::QueryAs { .. } => {
                        panic!("ensure_channel used an authenticated read unexpectedly")
                    }
                }
            }
        })
    }

    fn channel(owner: Option<Vec<u8>>) -> chat::Channel {
        chat::Channel {
            id: "term-0000000000000001".into(),
            name: "term-0000000000000001".into(),
            created_at: 0,
            head_seq: 0,
            post_policy: chat::PostPolicy::Open,
            hooks: Vec::new(),
            pinned: Vec::new(),
            huddle: Vec::new(),
            voice: false,
            owner: owner.map_or(Party::System, Party::Key),
            revision: 1,
            archived: false,
        }
    }

    #[tokio::test]
    async fn ensure_channel_is_ready_on_a_fresh_create() {
        let (handle, rx, _hub) = NodeHandle::channel();
        let actor = spawn_chat_actor(rx, Ok(()), None);
        assert!(matches!(
            ensure_channel(&handle, "term-0000000000000001").await,
            EnsureChannelOutcome::Ready
        ));
        drop(handle);
        actor.await.expect("actor task");
    }

    #[tokio::test]
    async fn ensure_channel_confirms_ownership_after_losing_the_create_race() {
        // the create lost the race ("already exists"), but the READ back says
        // the existing channel's owner is `DEFAULT_ORIGIN` — the same account
        // `NodeHandle::channel()`'s bare test handle (no mesh identity)
        // resolves to. This node's own account already IS the channel's
        // owner. Ready, same as a fresh create — and no write was made.
        let (handle, rx, _hub) = NodeHandle::channel();
        let actor = spawn_chat_actor(
            rx,
            Err("channel already exists: term-0000000000000001"),
            Some(crate::DEFAULT_ORIGIN.as_bytes().to_vec()),
        );
        assert!(matches!(
            ensure_channel(&handle, "term-0000000000000001").await,
            EnsureChannelOutcome::Ready
        ));
        drop(handle);
        actor.await.expect("actor task");
    }

    /// **#1746, the regression.** A create loss ("already exists") used to be
    /// treated as success outright, with no check of who actually won it — so
    /// a member who pre-squatted `term-<id>` became the session's command
    /// author. Now the READ-back owner is compared against this node's own
    /// account, and a mismatch surfaces as `Squatted`, which `create_local`
    /// closes the session on rather than spawning a projector that would
    /// trust the squatter as `Channel.owner`.
    #[tokio::test]
    async fn ensure_channel_refuses_a_channel_squatted_by_another_account() {
        let (handle, rx, _hub) = NodeHandle::channel();
        let actor = spawn_chat_actor(
            rx,
            Err("channel already exists: term-0000000000000001"),
            Some(vec![0x99, 0x99]),
        );
        assert!(matches!(
            ensure_channel(&handle, "term-0000000000000001").await,
            EnsureChannelOutcome::Squatted("channel_owned_by_another_account")
        ));
        drop(handle);
        actor.await.expect("actor task");
    }

    #[tokio::test]
    async fn existing_account_owner_is_checked_against_the_actual_signers_current_account() {
        for resolved in [Some(7), Some(8), None] {
            let (handle, mut rx, _hub) = NodeHandle::channel();
            handle.status_cell().publish(crate::NodeStatus {
                public_key: "ab".repeat(32),
                ..Default::default()
            });
            let actor = tokio::spawn(async move {
                let NodeCommand::Query { target, req, reply } =
                    rx.next().await.expect("channel query")
                else {
                    panic!("only reads are allowed");
                };
                assert_eq!(target, "chat");
                assert!(matches!(
                    chat::decode_query(&req).unwrap(),
                    chat::ChatQuery::Channel { .. }
                ));
                let mut owned = channel(None);
                owned.owner = Party::Account(7);
                reply
                    .send(Ok(chat::encode_reply(&chat::ChatReply::Channel(Some(
                        owned,
                    )))))
                    .unwrap();
                let NodeCommand::Query { target, req, reply } =
                    rx.next().await.expect("identity query")
                else {
                    panic!("only reads are allowed");
                };
                assert_eq!(target, "identity");
                assert_eq!(
                    identity::decode_query(&req).unwrap(),
                    identity::IdentityQuery::OfKey {
                        key: vec![0xab; 32]
                    }
                );
                let account = resolved.map(|number| identity::AccountView {
                    number,
                    name: String::new(),
                    control: identity::Control::Keys,
                    keys: vec![identity::KeyView {
                        scheme: identity::KeyScheme::Ed25519,
                        pubkey: vec![0xab; 32],
                        label: None,
                        added_at: 0,
                    }],
                    avatar: None,
                    bio: None,
                    updated_at: 0,
                });
                reply
                    .send(Ok(identity::encode_reply(
                        &identity::IdentityReply::Account(account),
                    )))
                    .unwrap();
            });
            let result = confirm_ownership(&handle, "term-0000000000000001").await;
            match resolved {
                Some(7) => assert!(matches!(result, EnsureChannelOutcome::Ready)),
                Some(_) | None => assert!(matches!(
                    result,
                    EnsureChannelOutcome::Squatted("channel_owned_by_another_account")
                )),
            }
            actor.await.unwrap();
        }
    }

    #[tokio::test]
    async fn ensure_channel_passes_through_a_transient_failure() {
        // NOT an authorization outcome — an actor/transport failure never
        // reaches `confirm_ownership` and must not be mistaken for a squat.
        let (handle, rx, _hub) = NodeHandle::channel();
        let actor = spawn_chat_actor(rx, Err("host busy"), None);
        assert!(matches!(
            ensure_channel(&handle, "term-0000000000000001").await,
            EnsureChannelOutcome::Failed(reason) if reason == "host busy"
        ));
        drop(handle);
        actor.await.expect("actor task");
    }

    #[test]
    fn self_key_falls_back_to_default_origin_with_no_mesh_identity() {
        // the bare test handle publishes no `NodeStatus` (empty `public_key`)
        // — the embedded-local-daemon shape `ensure_channel`'s doc describes.
        let (handle, _rx, _hub) = NodeHandle::channel();
        assert_eq!(self_key(&handle), crate::DEFAULT_ORIGIN.as_bytes());
    }

    #[test]
    fn self_key_reads_the_published_mesh_identity_when_present() {
        let (handle, _rx, _hub) = NodeHandle::channel();
        handle.status_cell().publish(crate::NodeStatus {
            public_key: "ab".repeat(32),
            ..Default::default()
        });
        assert_eq!(self_key(&handle), vec![0xab; 32]);
    }
}
