//! the `collaboration` module — the DELIVERY side of agent messaging.
//!
//! People and agents exchange explicit messages and results across devices
//! without knowing each other's IP, local path or provider session id. The
//! conversation is a chat channel and every message is a chat message; this
//! module owns the half chat cannot: bindings (which device carries a
//! participant's mail), per-recipient delivery records and their bounded
//! mailbox accounting.
//!
//! ## what it deliberately does NOT own
//!
//! * **Content.** No body, topic or roster is stored here. A delivery names a
//!   chat message by id, and the recipient must be able to read the channel
//!   by chat's own rule (`ChatQuery::Access`).
//! * **Managed work.** A delivery may REFERENCE a `tasks` job and the attempt
//!   its sender meant; this module reads that record to fence a stale target
//!   and never creates, claims or schedules one.
//! * **Provider adapters.** Local socket paths, provider session ids and
//!   credentials never become network addresses. A [`Binding`] carries an
//!   opaque device LABEL and a scoped public key, nothing else.
//! * **Semantic acceptance.** [`DeliveryState::AdapterAccepted`] says a
//!   provider input interface took the input. It does not say a model read it,
//!   understood it, acted on it, or claimed anything.
//!
//! ## storage
//!
//! QMDB-BACKED: pure logic over a host-injected [`sdk::MerkleStore`] with the
//! shared [`StagedStore`] overlay in front of it, the `tasks` shape. Every
//! record is its own store key, `root()` is the store's cached merkle root,
//! and state sync rides the store's resolver lane. See [`store`] for the key
//! space.
//!
//! ## authority
//!
//! Writes resolve their actor from `sdk::Env::origin`; reads resolve their
//! caller from the query context's origin (see [`read`]). Nothing on the wire
//! asserts an identity — a participant is a chat [`Party`], and the module
//! verifies that the caller IS that party or holds its binding.

// the wire surface: this module's shared types, flattened at the crate root.
pub use collaboration_wire::*;
// the wire crate under the name the module's own files reach it by.
pub(crate) use collaboration_wire as interface;

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;

mod bindings;
mod delivery;
mod read;
mod store;

pub use delivery::{MAX_REASON_BYTES, QUEUE_FULL};
pub use store::MAX_RECORD_BYTES;

use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore,
    StateRoot, StateSyncHandle,
};

/// the collaboration module over one store.
pub struct Collaboration {
    id: ModuleId,
    identity: ModuleId,
    tasks: ModuleId,
    chat: ModuleId,
    /// the ceiling on a delivery deadline, IN THIS NETWORK'S `consensus_time`
    /// unit. a constructor parameter and not a constant, because
    /// `consensus_time` is a block height on the validator lanes and a
    /// millisecond epoch clock on the sim lane: one number cannot mean seven
    /// days in both. The network states its unit as the `time_unit` genesis
    /// parameter and [`crate::max_delivery_ttl`] scales
    /// [`crate::MAX_DELIVERY_TTL_SECONDS`] into it.
    max_delivery_ttl: u64,
    /// this network's `chain_id`. Every op names the network it was authorized
    /// for and must name THIS one: a submitted frame's signature covers no
    /// chain id (`node::frame_preimage`), so without this the same signed
    /// bytes would replay on every network the signer can reach.
    network: String,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes; folded into `root()` at `commit_block`).
    staged: StagedStore,
}

impl Collaboration {
    pub fn new(
        id: impl Into<ModuleId>,
        identity: impl Into<ModuleId>,
        tasks: impl Into<ModuleId>,
        chat: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
        max_delivery_ttl: u64,
        network: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            identity: identity.into(),
            tasks: tasks.into(),
            chat: chat.into(),
            max_delivery_ttl,
            network: network.into(),
            staged: StagedStore::new(store),
        }
    }

    /// the network binding every op must carry. An EMPTY configured id is not
    /// a wildcard — it refuses everything, because an empty id matching an
    /// empty `Request::network` would collapse every network into one.
    fn check_network(&self, network: &str) -> Result<(), Error> {
        if self.network.is_empty() {
            return Err(Error::Module(
                "this module was composed with a blank chain_id, which binds no network".into(),
            ));
        }
        if network != self.network {
            return Err(Error::Module(format!(
                "op is bound to network {network:?}, not this network"
            )));
        }
        Ok(())
    }
}

/// The actor is an account when identity knows it, otherwise the
/// authenticated key or module remains a distinct non-account principal — the
/// `chat` convention verbatim, so one origin resolves to the same party here
/// as it does when it posts.
pub(crate) async fn actor_from_origin(ctx: &dyn Ctx, identity: &str) -> Result<Party, Error> {
    match &ctx.env().origin {
        Origin::Program(account) => {
            let reply = identity_reply(
                ctx,
                identity,
                identity::IdentityQuery::Get { number: *account },
            )
            .await?;
            let identity::IdentityReply::Account(Some(account)) = reply else {
                return Err(Error::Module("program account does not exist".into()));
            };
            let is_active_program = matches!(
                account.control,
                identity::Control::Program {
                    standing: identity::ProgramStanding::Active,
                    ..
                }
            );
            if !is_active_program {
                return Err(Error::Module("program account is not active".into()));
            }
            Ok(Party::Account(account.number))
        }
        Origin::External(key) => {
            if key.is_empty() {
                return Err(Error::Module(
                    "external origin must carry a non-empty submitter id".into(),
                ));
            }
            let reply = identity_reply(
                ctx,
                identity,
                identity::IdentityQuery::OfKey { key: key.clone() },
            )
            .await?;
            let identity::IdentityReply::Account(account) = reply else {
                return Err(Error::Module(
                    "identity returned an unexpected reply".into(),
                ));
            };
            Ok(account.map_or_else(
                || Party::Key(key.clone()),
                |account| Party::Account(account.number),
            ))
        }
        Origin::Module(module) => Ok(Party::Module(module.clone())),
        Origin::System => Ok(Party::System),
    }
}

async fn identity_reply(
    ctx: &dyn Ctx,
    identity: &str,
    query: identity::IdentityQuery,
) -> Result<identity::IdentityReply, Error> {
    let bytes = ctx.query(identity, &identity::encode_query(&query)).await?;
    identity::decode_reply(&bytes).map_err(Error::Module)
}

/// one party's standing in one channel, by chat's own gate. an unknown
/// channel answers all-false, so every caller fails closed on it.
pub(crate) async fn chat_access(
    ctx: &dyn Ctx,
    chat: &str,
    channel_id: &str,
    party: &Party,
) -> Result<chat::ChannelAccess, Error> {
    let query = chat::ChatQuery::Access {
        channel_id: channel_id.to_string(),
        party: party.clone(),
    };
    let bytes = ctx.query(chat, &chat::encode_query(&query)).await?;
    let chat::ChatReply::Access(access) = chat::decode_reply(&bytes).map_err(Error::Module)? else {
        return Err(Error::Module("chat returned an unexpected reply".into()));
    };
    Ok(access)
}

/// one chat message by its client-minted id, wherever it is.
pub(crate) async fn chat_message(
    ctx: &dyn Ctx,
    chat: &str,
    message_id: &str,
) -> Result<Option<chat::MessageView>, Error> {
    let query = chat::ChatQuery::Message {
        message_id: message_id.to_string(),
    };
    let bytes = ctx.query(chat, &chat::encode_query(&query)).await?;
    let chat::ChatReply::Message(view) = chat::decode_reply(&bytes).map_err(Error::Module)? else {
        return Err(Error::Module("chat returned an unexpected reply".into()));
    };
    Ok(view)
}

impl Collaboration {
    /// ONE dispatch: each arm delegates to the handler named for its variant.
    /// No wildcard — a new variant must fail the build until it is routed.
    async fn on_msg(&mut self, ctx: &mut dyn Ctx, msg: CollaborationMsg) -> Result<(), Error> {
        let actor = actor_from_origin(ctx, &self.identity).await?;
        let origin = ctx.env().origin.clone();
        let now = ctx.env().consensus_time;
        match msg {
            CollaborationMsg::Bind {
                channel_id,
                participant,
                device,
                principal,
                expected_credential,
            } => {
                let advanced = bindings::bind(
                    &mut self.staged,
                    ctx,
                    &self.chat,
                    now,
                    channel_id.clone(),
                    participant,
                    device,
                    principal,
                    expected_credential,
                )
                .await?;
                self.stamp(ctx, actor, channel_id, advanced.seq);
                Ok(())
            }
            CollaborationMsg::Unbind {
                channel_id,
                participant,
                expected_credential,
            } => {
                let advanced = bindings::unbind(
                    &mut self.staged,
                    ctx,
                    &self.chat,
                    now,
                    channel_id.clone(),
                    participant,
                    expected_credential,
                )
                .await?;
                self.stamp(ctx, actor, channel_id, advanced.seq);
                Ok(())
            }
            CollaborationMsg::Deliver(request) => {
                let channel_id = request.channel_id.clone();
                let requested = delivery::deliver(
                    &mut self.staged,
                    ctx,
                    &actor,
                    &origin,
                    now,
                    self.max_delivery_ttl,
                    &self.tasks,
                    &self.chat,
                    request,
                )
                .await?;
                // an idempotent repeat stages nothing and notifies nobody: the
                // channel did not advance, and re-announcing would make a
                // relay's retry look like new mail.
                if requested.replayed {
                    ctx.set_output(encode_reply(&CollaborationReply::Delivery(Some(
                        requested.delivery,
                    ))));
                    return Ok(());
                }
                self.stamp(ctx, actor, channel_id, requested.seq);
                Ok(())
            }
            CollaborationMsg::Acknowledge {
                channel_id,
                seq,
                recipient,
                binding_credential,
                state,
                reason,
            } => {
                let advanced = delivery::acknowledge(
                    &mut self.staged,
                    ctx,
                    now,
                    &self.tasks,
                    &self.chat,
                    channel_id.clone(),
                    seq,
                    recipient,
                    binding_credential,
                    state,
                    reason,
                )
                .await?;
                self.stamp(ctx, actor, channel_id, advanced.seq);
                Ok(())
            }
            CollaborationMsg::ExpireMessage {
                channel_id,
                seq,
                recipient,
            } => {
                let advanced =
                    delivery::expire(&mut self.staged, now, channel_id.clone(), seq, recipient)
                        .await?;
                self.stamp(ctx, actor, channel_id, advanced.seq);
                Ok(())
            }
        }
    }

    /// the channel's event stream moved: stamp what the module assigned, and
    /// emit the cursor HINT. the authoritative body is the committed event
    /// stream a consumer fetches after its last acknowledged sequence — this
    /// says only "there is something past your cursor".
    fn stamp(&self, ctx: &mut dyn Ctx, actor: Party, channel_id: String, seq: u64) {
        ctx.set_assigned(encode_assigned(&CollaborationAssigned::Advanced {
            channel_id: channel_id.clone(),
            seq,
            actor,
        }));
        ctx.set_output(encode_event(&CollaborationEvent::ChannelAdvanced {
            channel_id,
            seq,
        }));
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Collaboration {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        self.staged.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }

    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let request = decode_msg(&msg.payload).map_err(Error::Module)?;
        // the network binding is checked BEFORE anything is read or staged: a
        // foreign-network op is not a refused write, it is not this network's
        // op at all.
        self.check_network(&request.network)?;
        self.on_msg(ctx, request.op).await
    }

    /// there is no unauthenticated read path. `query` has no context, so it
    /// cannot authenticate anybody and answers nothing; every read arrives
    /// through [`Module::query_with`].
    async fn query(&self, _req: &[u8]) -> Result<Vec<u8>, Error> {
        Err(Error::QueryUnsupported)
    }

    async fn query_with(&self, ctx: &dyn Ctx, req: &[u8]) -> Result<Vec<u8>, Error> {
        let CollaborationQuery::Read {
            participant,
            via,
            read,
        } = decode_query(req).map_err(Error::Module)?;
        let reply = read::serve(
            &self.staged,
            ctx,
            &self.identity,
            &self.chat,
            &participant,
            via.as_deref(),
            read,
        )
        .await?;
        Ok(encode_reply(&reply))
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}
