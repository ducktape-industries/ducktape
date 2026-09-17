//! the shared harness: a module over an in-memory store, a FAKE chat sibling
//! holding channels, rosters and messages, plus the ctx constructors the
//! behaviour tests drive it with.
//!
//! `identity` answers "no account" by default, so an external key resolves to
//! `Party::Key(key)` — the non-account participant every test signs as unless
//! it deliberately registers an account. `tasks` answers with whatever job the
//! test installs, so attempt fencing is exercised against a real reply shape.
//! `chat` answers `Access` from the fake roster and `Message` from the fake
//! message table, so the module's two chat reads see real shapes too.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use collaboration::{
    BoundPrincipal, Collaboration, CollaborationMsg, CollaborationQuery, CollaborationReply,
    DeliverRequest, MessageKind, Party, ProtectedRead, encode_msg, encode_query,
};
use sdk::{Cause, Env, Error, Module, Msg, Origin};
use sdk_testkit::{MemStore, TestCtx};

pub const MODULE: &str = "collaboration";
pub const IDENTITY: &str = "identity";
pub const TASKS: &str = "tasks";
pub const CHAT: &str = "chat";
/// the height-lane ceiling every test composes with, so a deadline reads in
/// the same units the assertions use.
pub const MAX_TTL: u64 = collaboration::max_delivery_ttl(sdk::genesis_config::TimeUnit::Height);
/// the network every op in these tests is bound to.
pub const NETWORK: &str = "test-net";

pub fn module() -> Collaboration {
    module_on(NETWORK, MAX_TTL)
}

/// a module composed for another network, or another delivery ceiling — the
/// two genesis parameters a fixed component cannot compile in.
pub fn module_on(network: &str, max_delivery_ttl: u64) -> Collaboration {
    Collaboration::new(
        MODULE,
        IDENTITY,
        TASKS,
        CHAT,
        Box::new(MemStore::new()),
        max_delivery_ttl,
        network,
    )
}

/// an authenticated external signer: 32 bytes of one value, the shape a real
/// ed25519 origin has.
pub fn key(byte: u8) -> Vec<u8> {
    vec![byte; 32]
}

pub fn party(byte: u8) -> Party {
    Party::Key(key(byte))
}

// ---- the fake chat --------------------------------------------------------

/// one channel as the fake chat holds it: members-only, with a roster.
#[derive(Default, Clone)]
pub struct FakeChannel {
    pub members: BTreeSet<Party>,
}

/// what the fake chat knows. shared by every ctx a test builds off one
/// [`Scene`], so a message posted "in chat" is visible to every later op.
#[derive(Default)]
pub struct FakeChat {
    pub channels: BTreeMap<String, FakeChannel>,
    pub messages: BTreeMap<String, chat::MessageView>,
    next_seq: BTreeMap<String, u64>,
}

impl FakeChat {
    pub fn channel(&mut self, id: &str, members: &[Party]) {
        self.channels.insert(
            id.into(),
            FakeChannel {
                members: members.iter().cloned().collect(),
            },
        );
    }

    /// post one message as `origin`, the way chat records it: the author is
    /// the party the origin resolves to (a bare key here) and `origin` is the
    /// exact signer.
    pub fn post(&mut self, channel_id: &str, message_id: &str, origin: Origin) -> u64 {
        self.post_in_thread(channel_id, message_id, origin, None)
    }

    pub fn post_in_thread(
        &mut self,
        channel_id: &str,
        message_id: &str,
        origin: Origin,
        thread: Option<u64>,
    ) -> u64 {
        let seq = self.next_seq.entry(channel_id.into()).or_insert(1);
        let assigned = *seq;
        *seq += 1;
        let author = match &origin {
            Origin::External(key) => Party::Key(key.clone()),
            Origin::Program(account) => Party::Account(*account),
            Origin::Module(id) => Party::Module(id.clone()),
            Origin::System => Party::System,
        };
        self.messages.insert(
            message_id.into(),
            chat::MessageView {
                channel_id: channel_id.into(),
                seq: assigned,
                head: chat::MessageHead {
                    message_id: message_id.into(),
                    author,
                    origin: origin.clone(),
                    content_origin: origin,
                    blocks: vec![chat::Block::paragraph("ping")],
                    created_at: 1,
                    rev: 0,
                    revision: 1,
                    edited_at: None,
                    base_rev: None,
                    deleted: false,
                    thread,
                    reply_count: 0,
                    last_reply_seq: None,
                },
            },
        );
        assigned
    }

    fn answer(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let reply = match chat::decode_query(req).map_err(Error::Module)? {
            chat::ChatQuery::Access { channel_id, party } => {
                let standing = self.channels.get(&channel_id).is_some_and(|channel| {
                    channel.members.contains(&party) || !party.is_person()
                });
                chat::ChatReply::Access(chat::ChannelAccess {
                    may_read: standing,
                    may_post: standing,
                })
            }
            chat::ChatQuery::Message { message_id } => {
                chat::ChatReply::Message(self.messages.get(&message_id).cloned())
            }
            other => {
                return Err(Error::Module(format!(
                    "the fake chat does not serve {other:?}"
                )));
            }
        };
        Ok(chat::encode_reply(&reply))
    }
}

pub type Chat = Rc<RefCell<FakeChat>>;

/// a ctx at `now`, dispatching as `origin`, over `chat`.
pub fn at(chat: &Chat, now: u64, origin: Origin) -> TestCtx {
    with_job(chat, now, origin, None)
}

/// a ctx whose `identity` sibling knows ONE program account: the shape the
/// dispatch call lane's `Origin::Program(account)` resolves against.
pub fn as_program(chat: &Chat, now: u64, account: sdk::AccountNumber) -> TestCtx {
    let chat = chat.clone();
    TestCtx::with_env(Env {
        height: now,
        consensus_time: now,
        origin: Origin::Program(account),
        me: MODULE.into(),
        cause: Cause::Direct,
    })
    .on_query(IDENTITY, move |_| {
        Ok(identity::encode_reply(&identity::IdentityReply::Account(
            Some(program_account(account)),
        )))
    })
    .on_query(TASKS, |_| {
        Ok(tasks::encode_job_reply(&tasks::JobsReply::Job(None)))
    })
    .on_query(CHAT, move |req| chat.borrow().answer(req))
}

/// an ACTIVE program account executed by the `agent` module — what identity
/// holds for an account the call lane may run.
pub fn program_account(number: sdk::AccountNumber) -> identity::AccountView {
    identity::AccountView {
        number,
        name: format!("program-{number}"),
        control: identity::Control::Program {
            controller: 1,
            executor: "agent".into(),
            generation: 0,
            standing: identity::ProgramStanding::Active,
        },
        keys: Vec::new(),
        avatar: None,
        bio: None,
        updated_at: 0,
    }
}

/// a ctx whose `tasks` sibling answers with `job` for every job query — the
/// attempt fence's input.
pub fn with_job(chat: &Chat, now: u64, origin: Origin, job: Option<tasks::Job>) -> TestCtx {
    let chat = chat.clone();
    TestCtx::with_env(Env {
        height: now,
        consensus_time: now,
        origin,
        me: MODULE.into(),
        cause: Cause::Direct,
    })
    .on_query(IDENTITY, |_| {
        Ok(identity::encode_reply(&identity::IdentityReply::Account(
            None,
        )))
    })
    .on_query(TASKS, move |_| {
        Ok(tasks::encode_job_reply(&tasks::JobsReply::Job(job.clone())))
    })
    .on_query(CHAT, move |req| chat.borrow().answer(req))
}

/// a job record with `attempt`, enough for the attempt fence to read.
pub fn job(job_id: &str, attempt: u64) -> tasks::Job {
    tasks::Job {
        job_id: job_id.into(),
        execution: tasks::JobExecution::OneShot,
        conversation_id: format!("{job_id}:1"),
        previous_job_id: None,
        continuation_operation_id: None,
        controls: Vec::new(),
        reports: Vec::new(),
        native_history: None,
        kind: "review".into(),
        spec: "{}".into(),
        submitter: tasks::Party::System,
        status: tasks::JobStatus::Processing,
        attempt,
        claim: None,
        result: None,
        comments: Vec::new(),
        created_at_revision: 1,
        created_at_height: 1,
        updated_at_height: 1,
    }
}

/// wrap an op for THIS network — the binding every op carries.
pub fn msg(payload: CollaborationMsg) -> Msg {
    on_network(NETWORK, payload)
}

pub fn on_network(network: &str, payload: CollaborationMsg) -> Msg {
    Msg {
        target: MODULE.into(),
        payload: encode_msg(&collaboration::Request::new(network, payload)),
    }
}

/// run one op and commit the block, the way a host does on success.
pub async fn apply(
    module: &mut Collaboration,
    ctx: &mut TestCtx,
    payload: CollaborationMsg,
) -> Result<(), Error> {
    let result = module.execute(ctx, &msg(payload)).await;
    if result.is_ok() {
        module.commit_block().await?;
    } else {
        module.abort_block().await?;
    }
    result
}

/// apply and unwrap — the happy-path setup steps.
pub async fn ok(module: &mut Collaboration, ctx: &mut TestCtx, payload: CollaborationMsg) {
    apply(module, ctx, payload)
        .await
        .unwrap_or_else(|e| panic!("expected the op to be admitted: {e:?}"));
}

pub async fn read(
    module: &Collaboration,
    ctx: &TestCtx,
    participant: &Party,
    via: Option<&str>,
    read: ProtectedRead,
) -> CollaborationReply {
    let request = encode_query(&CollaborationQuery::Read {
        participant: participant.clone(),
        via: via.map(str::to_string),
        read,
    });
    let bytes = module
        .query_with(ctx, &request)
        .await
        .expect("a read answers, refusals included");
    collaboration::decode_reply(&bytes).expect("a reply decodes")
}

/// the credential a participant's current binding holds.
pub async fn credential_of(
    module: &Collaboration,
    ctx: &TestCtx,
    participant: &Party,
    channel_id: &str,
) -> u64 {
    let CollaborationReply::Binding(Some(view)) = read(
        module,
        ctx,
        participant,
        None,
        ProtectedRead::Binding {
            channel_id: channel_id.into(),
        },
    )
    .await
    else {
        panic!("{participant:?} holds a binding on {channel_id}");
    };
    view.credential
}

// ---- op builders ----------------------------------------------------------

pub fn bind(
    channel_id: &str,
    participant: &Party,
    service_key: Vec<u8>,
    expected_credential: u64,
) -> CollaborationMsg {
    bind_to(
        channel_id,
        participant,
        BoundPrincipal::ServiceKey(service_key),
        expected_credential,
    )
}

/// bind an arbitrary principal — the program-account arm reaches this module
/// over the dispatch call lane, not over a signature.
pub fn bind_to(
    channel_id: &str,
    participant: &Party,
    principal: BoundPrincipal,
    expected_credential: u64,
) -> CollaborationMsg {
    CollaborationMsg::Bind {
        channel_id: channel_id.into(),
        participant: participant.clone(),
        device: "laptop".into(),
        principal,
        expected_credential,
    }
}

/// a minimal notice delivery of chat message `message_id` to `recipient`,
/// expiring at `expires_at`.
pub fn deliver(
    channel_id: &str,
    message_id: &str,
    recipient: &Party,
    expires_at: u64,
) -> DeliverRequest {
    DeliverRequest {
        channel_id: channel_id.into(),
        message_id: message_id.into(),
        recipient: recipient.clone(),
        kind: MessageKind::Notice,
        task: None,
        references: Vec::new(),
        expires_at,
    }
}

/// the whole setup every delivery test starts from: two participants (the
/// bare keys 1 and 2), both members of chat channel `c1`.
pub struct Scene {
    pub module: Collaboration,
    pub chat: Chat,
    pub alice: Party,
    pub bob: Party,
}

pub fn scene(channel_id: &str) -> Scene {
    let chat: Chat = Rc::new(RefCell::new(FakeChat::default()));
    let alice = party(1);
    let bob = party(2);
    chat.borrow_mut()
        .channel(channel_id, &[alice.clone(), bob.clone()]);
    Scene {
        module: module(),
        chat,
        alice,
        bob,
    }
}

impl Scene {
    /// alice posts `message_id` on `channel_id`; the chat sequence it took.
    pub fn alice_posts(&self, channel_id: &str, message_id: &str) -> u64 {
        self.chat
            .borrow_mut()
            .post(channel_id, message_id, Origin::External(key(1)))
    }

    pub fn as_alice(&self, now: u64) -> TestCtx {
        at(&self.chat, now, Origin::External(key(1)))
    }

    pub fn as_bob(&self, now: u64) -> TestCtx {
        at(&self.chat, now, Origin::External(key(2)))
    }

    pub fn as_key(&self, now: u64, byte: u8) -> TestCtx {
        at(&self.chat, now, Origin::External(key(byte)))
    }
}
