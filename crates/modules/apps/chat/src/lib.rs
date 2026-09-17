//! qmdb-backed chat module: block-based channels, threads, edits, tombstones,
//! reactions, membership, and hook notifications.
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: the HOST constructs
//! the concrete store (qmdb today — `statesync::qmdb::QmdbStore`) and hands it
//! to [`Chat::new`], so this crate never names a storage crate. the store is
//! used for what it is — hash-addressable authenticated state, one logical
//! record per entity, every read a point lookup the DISPATCH path needs:
//! per-channel records, one record per message head, immutable per-edit
//! revision records, per-emoji reaction sets (plus the bounded per-message
//! emoji index the caps and tombstone cleanup require), message-id pointers
//! for global dedup, and membership records for the post policy. no stored
//! enumeration lists, no stand-in range indexes: everything a human scrolls,
//! lists, or searches is served by the index guest (`index.rs`) on the
//! derived tier. the one computed-key iteration that remains — the
//! `MessagesRange` context window over the gap-free `head_seq` space — exists
//! because CONSENSUS consumers (runs, automations) read it in `execute()`,
//! and consensus can never depend on the unverifiable derived tier.
//!
//! ## parties
//!
//! every write acts as ONE [`Party`], derived from `ctx.env().origin` and
//! never from a payload: an external key resolves through the identity
//! sibling to the account holding it, and stays a bare [`Party::Key`] when
//! identity knows none (a node operating a channel under its own key); a
//! program origin is its account; a module is itself; the system is the
//! system. an empty external origin (the pre-consensus default) is rejected.
//! rosters and relations name parties in that same resolved vocabulary.
//!
//! ## attribution
//!
//! chat is a SOURCE for the attribution plane: a channel is an object whose
//! relation set is its owner's ownership, a message one whose set is its
//! author's authorship plus one mention per mentioned account. every create,
//! edit and delete of either reports the object's FULL set at a new,
//! strictly increasing per-object revision (`Channel::revision`,
//! `MessageHead::revision`) in the same unit as the write, so the write and
//! its attribution commit or abort together; a delete reports the empty set.
//! only accounts are recipients: a key, module or system author holds no
//! relation, though it is still the report's actor.
//!
//! like `document` and `kv`, writes are staged in memory during a block and
//! flushed to the store in one batch at `commit_block`; the module root IS
//! the store's merkle root. sync belongs to the store, not this module: a
//! joiner rebuilds the concrete store from a peer (`QmdbStore::sync_from`)
//! and wraps a fresh `Chat` around it — this module only forwards the trait's
//! serve surface.

// the wire surface: this module's shared types, flattened at the crate root.
pub use chat_wire::*;
use chat_wire::client::validate_channel_namespace;

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;
// everything below is OFF-consensus: none of it touches qmdb or the
// root-hash, and the index engine's deps (fluent31 IO) cannot cross into the
// wasm guest — so the consensus state machine above compiles for wasm32
// without them. (The call media planes live in the `media-service` crate.)
//
// the derived-tier materialized view (`index`: the PURE fold + view over
// index_guest::StateRead) and the CLIENT view model (`client`: rendered row
// types, composer parsing, optimistic merges, the op-delta fold) are the
// wire crate's — every reader of the feed links them without the module.
// the engine shell that runs the fold inside the module's index database is
// `index_guest` below.

// the wasm index-mapper shell: wires the pure core into the fluent31 engine.
// compiled only by `guest-builder --index`'s synthesized wasm32 workspace
// (feature `index-guest`), never by the native build.
#[cfg(feature = "index-guest")]
mod index_guest;

use std::collections::{BTreeMap, BTreeSet};

use attribution::{
    Actor, AttributionMsg, ObjectRef, Reason, Relation, encode_msg as attribution_encode_msg,
};
use identity::{
    IdentityQuery, IdentityReply, decode_reply as identity_decode_reply,
    encode_query as identity_encode_query,
};
use sdk::{
    AccountNumber, Ctx, Error, KEY_SEP, MerkleStore, Module, ModuleId, Msg, Origin,
    ResolverSyncTarget, StagedStore, StateRoot, StateSyncHandle, require_non_empty,
};
use serde::{Serialize, de::DeserializeOwned};

/// single-component key: prefix + 0 + id. safe because every prefix is a fixed
/// literal and no prefix is another prefix followed by a 0 byte.
fn keyed(prefix: &[u8], id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + 1 + id.len());
    key.extend_from_slice(prefix);
    key.push(0);
    key.extend_from_slice(id.as_bytes());
    key
}

/// length-prefixed component for multi-part keys — never a 0-byte separator,
/// so components containing separators cannot collide.
fn component(key: &mut Vec<u8>, value: &[u8]) {
    key.extend_from_slice(&(value.len() as u64).to_be_bytes());
    key.extend_from_slice(value);
}

/// the canonical bytes of a party inside a composite key — borsh, so two
/// distinct parties never share bytes and a key can never spell an account.
fn party_bytes(party: &Party) -> Vec<u8> {
    borsh::to_vec(party).expect("a party is borsh-serializable")
}

fn channel_key(channel_id: &str) -> Vec<u8> {
    keyed(b"channel", channel_id)
}

fn msg_key(channel_id: &str, seq: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + 8 + channel_id.len() + 8);
    key.extend_from_slice(b"msg");
    component(&mut key, channel_id.as_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn rev_key(channel_id: &str, seq: u64, rev: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(3 + 8 + channel_id.len() + 12);
    key.extend_from_slice(b"rev");
    component(&mut key, channel_id.as_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key.extend_from_slice(&rev.to_be_bytes());
    key
}

fn react_key(channel_id: &str, seq: u64, emoji: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(5 + 16 + channel_id.len() + 8 + emoji.len());
    key.extend_from_slice(b"react");
    component(&mut key, channel_id.as_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    component(&mut key, emoji.as_bytes());
    key
}

/// per-message emoji index: hashed keys cannot enumerate `react/...`, so the
/// distinct-emoji list (bounded by [`MAX_REACTION_EMOJIS`]) is its own record.
fn reactidx_key(channel_id: &str, seq: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(8 + 8 + channel_id.len() + 8);
    key.extend_from_slice(b"reactidx");
    component(&mut key, channel_id.as_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn msgid_key(message_id: &str) -> Vec<u8> {
    keyed(b"msgid", message_id)
}

/// The two accounts of a DM channel, stored at its creation: the id is a
/// digest of them, so nothing else can name the other side of a post.
fn dm_pair_key(channel_id: &str) -> Vec<u8> {
    keyed(b"dmpair", channel_id)
}

fn member_key(channel_id: &str, party: &Party) -> Vec<u8> {
    let party = party_bytes(party);
    let mut key = Vec::with_capacity(6 + 16 + channel_id.len() + party.len());
    key.extend_from_slice(b"member");
    component(&mut key, channel_id.as_bytes());
    component(&mut key, &party);
    key
}

/// one creator's channel-creation counter — what [`MAX_CHANNELS_PER_CREATOR`]
/// is checked against.
fn creator_count_key(party: &Party) -> Vec<u8> {
    let party = party_bytes(party);
    let mut key = Vec::with_capacity(9 + 8 + party.len());
    key.extend_from_slice(b"chancount");
    component(&mut key, &party);
    key
}

/// an id that names an attribution object (a channel id, a message id):
/// non-empty and free of the plane's reserved separator, so chat's report of
/// it can never alias another object's.
fn validate_object_id(field: &str, value: &str) -> Result<(), Error> {
    require_non_empty(field, value)?;
    if value.contains(KEY_SEP) {
        return Err(Error::Module(format!(
            "{field} must not contain the reserved separator"
        )));
    }
    Ok(())
}

/// the structured mention parties of message blocks, first occurrence order,
/// deduplicated — the parties a post NAMES, before resolution.
fn collect_mentions(blocks: &[Block]) -> Vec<Party> {
    let mut mentions: Vec<Party> = Vec::new();
    for block in blocks {
        let spans = match block {
            Block::Paragraph(spans) | Block::Quote(spans) => spans,
            Block::Code { .. } | Block::Divider => continue,
        };
        for span in spans {
            for mark in &span.marks {
                if let Mark::Mention(party) = mark
                    && !mentions.contains(party)
                {
                    mentions.push(party.clone());
                }
            }
        }
    }
    mentions
}

fn clamp_limit(limit: u64) -> u64 {
    limit.min(MAX_QUERY_LIMIT)
}

// ---- the attribution edge ---------------------------------------------------
// chat's shapes translated into the plane's vocabulary, at this edge only.

/// one attribution report a write produced: the object's FULL relation set at
/// its new revision. decided by the stage fns beside the write that revises
/// the object; emitted by `execute` once the unit's writes are staged.
struct Report {
    object: ObjectRef,
    revision: u64,
    relations: Vec<Relation>,
}

fn channel_object(channel_id: &str) -> ObjectRef {
    ObjectRef {
        kind: OBJECT_KIND_CHANNEL.into(),
        object: channel_id.into(),
    }
}

fn message_object(message_id: &str) -> ObjectRef {
    ObjectRef {
        kind: OBJECT_KIND_MESSAGE.into(),
        object: message_id.into(),
    }
}

fn relation(recipient: AccountNumber, reason: Reason) -> Relation {
    Relation {
        recipient,
        reason,
        detail: Vec::new(),
    }
}

/// a channel's relation set: its owner's ownership, when the owner is an
/// account. a key, module or system owner holds no relation.
fn channel_relations(owner: &Party) -> Vec<Relation> {
    owner
        .account()
        .map(|account| relation(account, Reason::Ownership))
        .into_iter()
        .collect()
}

/// a live message's relation set: its author's authorship (when the author is
/// an account) plus one mention per mentioned account, plus the other side
/// of a DM as one more mention — a DM ADDRESSES ITS OTHER SIDE, so an agent
/// there is summoned the way an @-mention summons it in a room. `mentions`
/// is already deduplicated, so no `(recipient, reason)` repeats; the
/// counterpart is skipped when the post names them outright.
fn message_relations(
    author: &Party,
    mentions: &[AccountNumber],
    addressed: Option<AccountNumber>,
) -> Vec<Relation> {
    let authorship = author
        .account()
        .map(|account| relation(account, Reason::Authorship));
    let addressed = addressed.filter(|account| !mentions.contains(account));
    authorship
        .into_iter()
        .chain(
            mentions
                .iter()
                .map(|account| relation(*account, Reason::Mention)),
        )
        .chain(addressed.map(|account| relation(account, Reason::Mention)))
        .collect()
}

/// the plane's actor for a chat party — the same four cases, one to one.
fn actor_of(party: &Party) -> Actor {
    match party {
        Party::Account(account) => Actor::Account(*account),
        Party::Key(key) => Actor::Key(key.clone()),
        Party::Module(module) => Actor::Module(module.clone()),
        Party::System => Actor::System,
    }
}

/// Account attribution and proof of key ownership are separate facts: joining
/// an account never transfers an older key-owned record to its other keys.
struct Authority {
    party: Party,
    origin: Origin,
}

impl Authority {
    /// Prefer a current account entry; otherwise retain an existing entry
    /// owned by this exact signing key. Other account keys cannot claim it.
    fn participant<'a>(&self, parties: impl IntoIterator<Item = &'a Party>) -> Party {
        let parties: Vec<_> = parties.into_iter().collect();
        if parties.contains(&&self.party) {
            return self.party.clone();
        }
        let Origin::External(key) = &self.origin else {
            return self.party.clone();
        };
        let historical = Party::Key(key.clone());
        if parties.contains(&&historical) {
            return historical;
        }
        self.party.clone()
    }
}

struct MessageContent {
    blocks: Vec<Block>,
    mentions: Vec<AccountNumber>,
}

struct ResolvedMentions {
    accounts: Vec<AccountNumber>,
    key_mentions: Vec<AccountNumber>,
}

struct Posted {
    seq: u64,
    thread_root: Option<u64>,
    hooks: Vec<String>,
    report: Report,
}

/// storage-backed chat module.
pub struct Chat {
    id: ModuleId,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (logical-key -> staged write; `None` = delete; read-your-writes, folded
    /// into `root()` at `commit_block`). store key is `sha256(logical_key)`,
    /// owned by [`StagedStore`].
    staged: StagedStore,
    /// the attribution plane every channel and message write reports to (one
    /// `Attribute` follow-up per revised object, same unit). `None` = no
    /// plane on this host (tests, minimal registries): nothing is reported.
    attribution: Option<ModuleId>,
    /// the identity sibling every external origin resolves through (`OfKey`)
    /// and every named account is validated against (`Get`). `None` = no
    /// sibling on this host (tests, minimal registries): such a host knows no
    /// account, so every external key stays a key and no mention resolves.
    identity: Option<ModuleId>,
}

impl Chat {
    /// wrap the host-constructed store under module identity `id`. sync — the
    /// store arrives already opened (or already synced to a verified root).
    pub fn new(id: impl Into<ModuleId>, store: Box<dyn MerkleStore>) -> Self {
        Self {
            id: id.into(),
            staged: StagedStore::new(store),
            attribution: None,
            identity: None,
        }
    }

    /// report every channel and message revision to `attribution`.
    pub fn with_attribution(mut self, attribution: impl Into<ModuleId>) -> Self {
        self.attribution = Some(attribution.into());
        self
    }

    /// resolve external keys and validate named accounts through `identity`.
    pub fn with_identity(mut self, identity: impl Into<ModuleId>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    async fn get_raw(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        self.staged.get(key).await
    }

    async fn load<T>(&self, key: &[u8]) -> Result<Option<T>, Error>
    where
        T: DeserializeOwned,
    {
        match self.get_raw(key).await? {
            Some(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).map_err(|e| Error::Module(e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    fn store<T>(&mut self, key: Vec<u8>, value: &T)
    where
        T: Serialize,
    {
        self.staged.stage(
            key,
            serde_json::to_vec(value).expect("chat value is serializable"),
        );
    }

    /// stage a value only if its serialized size fits `cap` — the write-time
    /// guard against poison values (the qmdb codec cap is decode-only).
    fn store_bounded<T>(
        &mut self,
        key: Vec<u8>,
        value: &T,
        cap: usize,
        what: &str,
    ) -> Result<(), Error>
    where
        T: Serialize,
    {
        let bytes = serde_json::to_vec(value).expect("chat value is serializable");
        if bytes.len() > cap {
            return Err(Error::Module(format!(
                "{what} record too large: {} > {cap} bytes",
                bytes.len()
            )));
        }
        self.staged.stage(key, bytes);
        Ok(())
    }

    fn delete(&mut self, key: Vec<u8>) {
        self.staged.delete(key);
    }

    async fn channel(&self, channel_id: &str) -> Result<Option<Channel>, Error> {
        self.load(&channel_key(channel_id)).await
    }

    async fn require_channel(&self, channel_id: &str) -> Result<Channel, Error> {
        self.channel(channel_id)
            .await?
            .ok_or_else(|| Error::Module(format!("unknown channel: {channel_id}")))
    }

    fn store_channel(&mut self, channel: &Channel) -> Result<(), Error> {
        self.store_bounded(
            channel_key(&channel.id),
            channel,
            MAX_CHANNEL_RECORD_BYTES,
            "channel",
        )
    }

    async fn head(&self, channel_id: &str, seq: u64) -> Result<Option<MessageHead>, Error> {
        self.load(&msg_key(channel_id, seq)).await
    }

    async fn require_head(&self, channel_id: &str, seq: u64) -> Result<MessageHead, Error> {
        self.head(channel_id, seq)
            .await?
            .ok_or_else(|| Error::Module(format!("unknown message: {channel_id}/{seq}")))
    }

    fn store_head(&mut self, channel_id: &str, seq: u64, head: &MessageHead) -> Result<(), Error> {
        self.store_bounded(
            msg_key(channel_id, seq),
            head,
            MAX_MESSAGE_HEAD_BYTES,
            "message",
        )
    }

    async fn is_member(&self, channel_id: &str, party: &Party) -> Result<bool, Error> {
        Ok(self
            .get_raw(&member_key(channel_id, party))
            .await?
            .is_some())
    }

    /// The other side of a DM, when `channel_id` is one and `author` is one
    /// of its two accounts; `None` for a room, or for a poster the pair does
    /// not name (a module, a key on no account).
    async fn dm_counterpart(
        &self,
        channel_id: &str,
        author: &Party,
    ) -> Result<Option<AccountNumber>, Error> {
        let Some((first, second)) = self
            .load::<(AccountNumber, AccountNumber)>(&dm_pair_key(channel_id))
            .await?
        else {
            return Ok(None);
        };
        let counterpart = match author.account() {
            Some(account) if account == first => Some(second),
            Some(account) if account == second => Some(first),
            _ => None,
        };
        Ok(counterpart)
    }

    // ---- identity ---------------------------------------------------------
    // the ONE resolver every party and every named account goes through.

    async fn identity_reply(
        &self,
        ctx: &dyn Ctx,
        identity: &ModuleId,
        query: &IdentityQuery,
    ) -> Result<Option<AccountNumber>, Error> {
        let reply = ctx.query(identity, &identity_encode_query(query)).await?;
        match identity_decode_reply(&reply).map_err(Error::Module)? {
            IdentityReply::Account(account) => Ok(account.map(|view| view.number)),
            IdentityReply::Accounts(_) | IdentityReply::Resolved(_) | IdentityReply::Gen(_) => {
                Err(Error::Module("chat: unexpected identity reply".into()))
            }
        }
    }

    /// the account holding `key`, through identity's `OfKey`. `None` when
    /// identity knows no such key — or when this host wires no identity
    /// sibling, which knows no key at all.
    async fn account_of_key(
        &self,
        ctx: &dyn Ctx,
        key: &[u8],
    ) -> Result<Option<AccountNumber>, Error> {
        let Some(identity) = &self.identity else {
            return Ok(None);
        };
        self.identity_reply(ctx, identity, &IdentityQuery::OfKey { key: key.to_vec() })
            .await
    }

    /// whether account `number` exists. identity numbers accounts from 1, so
    /// 0 never exists; a host wiring no identity sibling has no accounts.
    async fn account_exists(&self, ctx: &dyn Ctx, number: AccountNumber) -> Result<bool, Error> {
        let Some(identity) = &self.identity else {
            return Ok(false);
        };
        if number == 0 {
            return Ok(false);
        }
        let found = self
            .identity_reply(ctx, identity, &IdentityQuery::Get { number })
            .await?;
        Ok(found.is_some())
    }

    /// the party the dispatch origin acts as — the only authorship path. an
    /// external key is the account holding it when identity knows one and the
    /// bare key otherwise; the pre-consensus default `Origin::External(vec![])`
    /// never passes as an authenticated party.
    async fn party_of_origin(&self, ctx: &dyn Ctx, origin: &Origin) -> Result<Party, Error> {
        match origin {
            Origin::External(key) if key.is_empty() => Err(Error::Module(
                "external origin must carry a non-empty submitter id".into(),
            )),
            Origin::External(key) => Ok(match self.account_of_key(ctx, key).await? {
                Some(account) => Party::Account(account),
                None => Party::Key(key.clone()),
            }),
            Origin::Module(id) => Ok(Party::Module(id.clone())),
            Origin::Program(account) => Ok(Party::Account(*account)),
            Origin::System => Ok(Party::System),
        }
    }

    /// the account a mention names, or the rejection: an account must exist,
    /// a key must hold an account, and a module or the system is no account.
    async fn resolve_mention_batch(
        &self,
        ctx: &dyn Ctx,
        mentions: &[Party],
    ) -> Result<Vec<AccountNumber>, Error> {
        if mentions.is_empty() {
            return Ok(Vec::new());
        }
        let references: Vec<_> = mentions
            .iter()
            .filter_map(|mention| match mention {
                Party::Account(number) => Some(identity::AccountRef::Account(*number)),
                Party::Key(key) => Some(identity::AccountRef::Key(key.clone())),
                Party::Module(_) | Party::System => None,
            })
            .collect();
        let numbers = match &self.identity {
            Some(identity) => {
                let bytes = ctx
                    .query(
                        identity,
                        &identity_encode_query(&IdentityQuery::Resolve { references }),
                    )
                    .await?;
                let IdentityReply::Resolved(numbers) =
                    identity_decode_reply(&bytes).map_err(Error::Module)?
                else {
                    return Err(Error::Module("chat: unexpected identity reply".into()));
                };
                numbers
            }
            None => vec![None; mentions.len()],
        };
        if numbers.len() != mentions.len() {
            return Err(Error::Module(
                "chat: identity resolution count mismatch".into(),
            ));
        }
        mentions
            .iter()
            .zip(numbers)
            .map(|(mention, number)| {
                number.ok_or_else(|| {
                    Error::Module(match mention {
                        Party::Account(account) => {
                            format!("chat: a mention names no account: {account}")
                        }
                        Party::Key(_) => "chat: a mentioned key belongs to no account".into(),
                        Party::Module(_) | Party::System => {
                            "chat: a mention names an account, never a module or the system".into()
                        }
                    })
                })
            })
            .collect()
    }

    /// the accounts `blocks` mention, resolved and deduplicated in first
    /// occurrence order. a mention that resolves to no account rejects the
    /// whole write.
    async fn resolve_mentions(
        &self,
        ctx: &dyn Ctx,
        blocks: &mut [Block],
    ) -> Result<ResolvedMentions, Error> {
        let mut accounts = Vec::new();
        let mut key_mentions = Vec::new();
        let mut resolved = BTreeMap::new();
        let mentions = collect_mentions(blocks);
        for chunk in mentions.chunks(identity::MAX_QUERY_LIMIT as usize) {
            let end = chunk
                .iter()
                .position(|party| !party.is_person())
                .unwrap_or(chunk.len());
            let numbers = self.resolve_mention_batch(ctx, &chunk[..end]).await?;
            for (mention, account) in chunk[..end].iter().zip(numbers) {
                if matches!(mention, Party::Key(_)) {
                    key_mentions.push(account);
                }
                resolved.insert(mention.clone(), account);
                if !accounts.contains(&account) {
                    accounts.push(account);
                }
            }
            if end != chunk.len() {
                return Err(Error::Module(
                    "chat: a mention names an account, never a module or the system".into(),
                ));
            }
        }
        for block in blocks {
            let spans = match block {
                Block::Paragraph(spans) | Block::Quote(spans) => spans,
                Block::Code { .. } | Block::Divider => continue,
            };
            for span in spans {
                for mark in &mut span.marks {
                    if let Mark::Mention(party) = mark {
                        *party = Party::Account(resolved[party]);
                    }
                }
            }
        }
        Ok(ResolvedMentions {
            accounts,
            key_mentions,
        })
    }

    /// a party a roster may name: a person, in the resolved vocabulary every
    /// poster arrives in — an account that exists, or a key holding none. a
    /// key that does hold an account is refused (the roster names the
    /// account, or the member's post would never match), and trusted code is
    /// never a member because it always may post.
    async fn validate_member(&self, ctx: &dyn Ctx, party: &Party) -> Result<(), Error> {
        match party {
            Party::Account(account) => {
                if !self.account_exists(ctx, *account).await? {
                    return Err(Error::Module(format!(
                        "chat: membership names no account: {account}"
                    )));
                }
                Ok(())
            }
            Party::Key(key) if key.is_empty() => {
                Err(Error::Module("chat: a member key must not be empty".into()))
            }
            Party::Key(key) => match self.account_of_key(ctx, key).await? {
                Some(account) => Err(Error::Module(format!(
                    "chat: this key belongs to account {account}; name the account"
                ))),
                None => Ok(()),
            },
            Party::Module(_) | Party::System => Err(Error::Module(
                "chat: modules and the system are never members; they always may post".into(),
            )),
        }
    }

    // ---- gates ------------------------------------------------------------

    /// gate a post/reaction on the channel policy. module and system parties
    /// always pass — modules are genesis-fixed trusted code; people need
    /// membership under `MembersOnly`.
    async fn check_post_policy(&self, channel: &Channel, party: &Party) -> Result<(), Error> {
        // an archived channel rejects posts, reactions, and huddle join/sweep:
        // every posting-class op routes through here, so one guard turns them
        // all away. edits and deletes deliberately do not call this — redacting
        // your own message stays possible in a closed channel — and neither do
        // membership, rename, or unarchive.
        if channel.archived {
            return Err(Error::Module(format!("channel {} is archived", channel.id)));
        }
        let gated_by_roster = channel.post_policy == PostPolicy::MembersOnly && party.is_person();
        if gated_by_roster && !self.is_member(&channel.id, party).await? {
            return Err(Error::Module(format!(
                "channel {} is members-only and the author is not a member",
                channel.id
            )));
        }
        Ok(())
    }

    async fn check_authorized_post(
        &self,
        channel: &Channel,
        authority: &Authority,
    ) -> Result<(), Error> {
        let error = match self.check_post_policy(channel, &authority.party).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let Origin::External(key) = &authority.origin else {
            return Err(error);
        };
        let key_holds_membership = !channel.archived
            && self
                .is_member(&channel.id, &Party::Key(key.clone()))
                .await?;
        if key_holds_membership {
            return Ok(());
        }
        Err(error)
    }

    /// one party's standing in one channel, answered from chat's own gates —
    /// [`ChatQuery::Access`]. a module that acts on a person's behalf
    /// (automations firing that person's rule) asks HERE instead of
    /// re-deriving the admission rule, because a second copy of it is a
    /// second rule. an unknown channel answers `false` to both: the caller
    /// fails closed.
    async fn channel_access(
        &self,
        channel_id: &str,
        party: &Party,
    ) -> Result<ChannelAccess, Error> {
        let Some(channel) = self.channel(channel_id).await? else {
            return Ok(ChannelAccess {
                may_read: false,
                may_post: false,
            });
        };
        // reading is not policied per-message: an OPEN channel is readable by
        // any authenticated party, a members-only one only by its members.
        let may_read_without_membership =
            channel.post_policy == PostPolicy::Open || !party.is_person();
        let may_read = may_read_without_membership || self.is_member(channel_id, party).await?;
        // the post answer is the post GATE, run verbatim — archival and policy
        // included — so the two can never drift apart.
        let may_post = self.check_post_policy(&channel, party).await.is_ok();
        Ok(ChannelAccess { may_read, may_post })
    }

    /// refuse channel creation once a person is at [`MAX_CHANNELS_PER_CREATOR`]
    /// — there is no `DeleteChannel` op, so this is the only thing bounding
    /// one party's share of the (permanent) channel set. trusted code is not
    /// counted.
    async fn check_creator_cap(&self, party: &Party) -> Result<(), Error> {
        if !party.is_person() {
            return Ok(());
        }
        let count: u64 = self.load(&creator_count_key(party)).await?.unwrap_or(0);
        if count as usize >= MAX_CHANNELS_PER_CREATOR {
            return Err(Error::Module(format!(
                "chat: you already have {MAX_CHANNELS_PER_CREATOR} channels open"
            )));
        }
        Ok(())
    }

    /// record that a person just created a channel — the counter
    /// [`Self::check_creator_cap`] reads.
    async fn bump_creator_count(&mut self, party: &Party) -> Result<(), Error> {
        if !party.is_person() {
            return Ok(());
        }
        let count: u64 = self.load(&creator_count_key(party)).await?.unwrap_or(0);
        self.store(creator_count_key(party), &(count + 1));
        Ok(())
    }

    // ---- channel writes ---------------------------------------------------

    /// stage a fresh channel record owned by `party` and report its first
    /// revision.
    fn stage_new_channel(
        &mut self,
        party: &Party,
        channel_id: String,
        name: String,
        post_policy: PostPolicy,
        voice: bool,
        created_at: u64,
    ) -> Result<Report, Error> {
        let channel = Channel {
            id: channel_id,
            name,
            created_at,
            head_seq: 0,
            post_policy,
            hooks: Vec::new(),
            pinned: Vec::new(),
            huddle: Vec::new(),
            voice,
            owner: party.clone(),
            archived: false,
            revision: 1,
        };
        self.store_channel(&channel)?;
        Ok(Report {
            object: channel_object(&channel.id),
            revision: channel.revision,
            relations: channel_relations(&channel.owner),
        })
    }

    async fn stage_channel(
        &mut self,
        party: &Party,
        channel_id: String,
        name: String,
        post_policy: PostPolicy,
        voice: bool,
        created_at: u64,
    ) -> Result<Report, Error> {
        validate_object_id("channel_id", &channel_id)?;
        require_non_empty("name", &name)?;
        validate_channel_namespace(party, &channel_id)?;
        // the `dm-` shape is reserved for `CreateDmChannel`, the only op that
        // derives the id from the creator's OWN account — a plain
        // `CreateChannel` naming that shape is exactly the squat this gate
        // closes (see the module doc on `CreateDmChannel`).
        if chat_wire::client::is_derived_dm_channel(&channel_id) {
            return Err(Error::Module(
                "chat: dm- channel ids are reserved; open a DM with CreateDmChannel".into(),
            ));
        }
        if self.channel(&channel_id).await?.is_some() {
            return Err(Error::Module(format!(
                "channel already exists: {channel_id}"
            )));
        }
        self.check_creator_cap(party).await?;
        let report =
            self.stage_new_channel(party, channel_id, name, post_policy, voice, created_at)?;
        self.bump_creator_count(party).await?;
        Ok(report)
    }

    /// open the two-party DM room with `counterpart`: derive the id from the
    /// creator's ACCOUNT (the origin's resolved party, never a payload) so
    /// only one of the pair may ever mint it, require the counterpart to be
    /// an account that exists, always seat `MembersOnly` regardless of what
    /// a squatter might otherwise request, and own it by its creator like
    /// any other person-made channel.
    async fn stage_dm_channel(
        &mut self,
        ctx: &dyn Ctx,
        party: &Party,
        counterpart: AccountNumber,
        name: String,
        created_at: u64,
    ) -> Result<(String, Report), Error> {
        let creator = match party {
            Party::Account(account) => *account,
            Party::Key(_) => {
                return Err(Error::Module(
                    "chat: this key belongs to no identity account".into(),
                ));
            }
            Party::Module(_) | Party::System => {
                return Err(Error::Module(
                    "chat: a DM channel must be opened by an account".into(),
                ));
            }
        };
        require_non_empty("name", &name)?;
        if creator == counterpart {
            return Err(Error::Module(
                "chat: a DM's two accounts must differ".into(),
            ));
        }
        if !self.account_exists(ctx, counterpart).await? {
            return Err(Error::Module(format!(
                "chat: a DM names no account: {counterpart}"
            )));
        }
        let channel_id = chat_wire::client::dm_channel_id(&creator.to_string(), &counterpart.to_string());
        if self.channel(&channel_id).await?.is_some() {
            return Err(Error::Module(format!(
                "channel already exists: {channel_id}"
            )));
        }
        self.check_creator_cap(party).await?;
        let report = self.stage_new_channel(
            party,
            channel_id.clone(),
            name,
            PostPolicy::MembersOnly,
            false,
            created_at,
        )?;
        self.store(member_key(&channel_id, party), &true);
        self.store(member_key(&channel_id, &Party::Account(counterpart)), &true);
        self.store(dm_pair_key(&channel_id), &(creator, counterpart));
        self.bump_creator_count(party).await?;
        Ok((channel_id, report))
    }

    /// stage a revised channel record and report the new revision.
    fn stage_channel_revision(&mut self, mut channel: Channel) -> Result<Report, Error> {
        channel.revision = channel
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::Module("channel revision exhausted".into()))?;
        self.store_channel(&channel)?;
        Ok(Report {
            object: channel_object(&channel.id),
            revision: channel.revision,
            relations: channel_relations(&channel.owner),
        })
    }

    /// rename a channel. any authenticated party renames any channel: a
    /// channel's owner is attribution, not a gate. reuses `CreateChannel`'s
    /// name validation (non-empty + the `:` namespace gate — the id is
    /// unchanged, but the gate still keeps a person off a module-namespaced
    /// channel) and the record byte cap. a same-name rename stages nothing
    /// and reports nothing.
    async fn stage_rename(
        &mut self,
        authority: &Authority,
        channel_id: &str,
        name: String,
    ) -> Result<Option<Report>, Error> {
        let party = &authority.party;
        require_non_empty("channel_id", channel_id)?;
        require_non_empty("name", &name)?;
        validate_channel_namespace(party, channel_id)?;
        let mut channel = self.require_channel(channel_id).await?;
        if channel.name == name {
            // idempotent: a same-name rename stages nothing, so the op log —
            // and the root — is byte-identical to no write at all.
            return Ok(None);
        }
        channel.name = name;
        Ok(Some(self.stage_channel_revision(channel)?))
    }

    /// archive or unarchive a channel. authorization mirrors `stage_rename`
    /// (the same `:` namespace gate keeps a person off a module-namespaced
    /// channel — otherwise any person could archive a `forge:<repo>:<n>`
    /// discussion, and an archived channel rejects the owning module's posts
    /// too); the flag itself is what `check_post_policy` reads to gate writes.
    async fn stage_set_archived(
        &mut self,
        authority: &Authority,
        channel_id: &str,
        archived: bool,
    ) -> Result<Option<Report>, Error> {
        let party = &authority.party;
        require_non_empty("channel_id", channel_id)?;
        validate_channel_namespace(party, channel_id)?;
        let mut channel = self.require_channel(channel_id).await?;
        if channel.archived == archived {
            return Ok(None);
        }
        channel.archived = archived;
        Ok(Some(self.stage_channel_revision(channel)?))
    }

    // ---- message writes ---------------------------------------------------

    async fn stage_message(
        &mut self,
        authority: &Authority,
        channel_id: &str,
        message_id: String,
        content: MessageContent,
        thread: Option<u64>,
        now: u64,
    ) -> Result<Posted, Error> {
        let author = authority.party.clone();
        let MessageContent { blocks, mentions } = content;
        require_non_empty("channel_id", channel_id)?;
        validate_object_id("message_id", &message_id)?;
        if blocks.is_empty() {
            return Err(Error::Module("blocks must not be empty".into()));
        }
        let mut channel = self.require_channel(channel_id).await?;
        self.check_authorized_post(&channel, authority).await?;
        if self.get_raw(&msgid_key(&message_id)).await?.is_some() {
            return Err(Error::Module(format!(
                "message already exists: {message_id}"
            )));
        }

        // the per-channel sequence comes from the head_seq counter — never
        // from the last element of a list (P3: gap-free, assigned in-state).
        let seq = channel.head_seq + 1;
        channel.head_seq = seq;

        if let Some(root_seq) = thread {
            // a reply is a normal message record with its own channel seq;
            // the root tracks the summary (`reply_count` is the reply-cap
            // authority — replies are enumerable from it as the root's
            // sequence-ordered descendants, so no stored reply list exists).
            // a tombstoned root still anchors its thread, so replying to it
            // stays legal.
            let mut root = self.require_head(channel_id, root_seq).await?;
            if root.thread.is_some() {
                return Err(Error::Module(format!(
                    "thread replies cannot start subthreads: {channel_id}/{root_seq}"
                )));
            }
            if root.reply_count >= MAX_THREAD_REPLIES as u64 {
                return Err(Error::Module(format!(
                    "thread reply cap reached: {channel_id}/{root_seq}"
                )));
            }
            root.reply_count += 1;
            root.last_reply_seq = Some(seq);
            self.store_head(channel_id, root_seq, &root)?;
        }

        let head = MessageHead {
            message_id: message_id.clone(),
            author,
            origin: authority.origin.clone(),
            content_origin: authority.origin.clone(),
            blocks,
            created_at: now,
            rev: 0,
            revision: 1,
            edited_at: None,
            base_rev: None,
            deleted: false,
            thread,
            reply_count: 0,
            last_reply_seq: None,
        };
        self.store_head(channel_id, seq, &head)?;
        self.store(msgid_key(&message_id), &(channel_id.to_string(), seq));
        let hooks = channel.hooks.clone();
        self.store_channel(&channel)?;
        let addressed = self.dm_counterpart(channel_id, &head.author).await?;
        Ok(Posted {
            seq,
            thread_root: thread,
            hooks,
            report: Report {
                object: message_object(&message_id),
                revision: head.revision,
                relations: message_relations(&head.author, &mentions, addressed),
            },
        })
    }

    async fn stage_edit(
        &mut self,
        authority: &Authority,
        channel_id: &str,
        seq: u64,
        content: MessageContent,
        base_rev: Option<u32>,
        now: u64,
    ) -> Result<(u32, Report), Error> {
        let MessageContent { blocks, mentions } = content;
        require_non_empty("channel_id", channel_id)?;
        if blocks.is_empty() {
            return Err(Error::Module("blocks must not be empty".into()));
        }
        let head = self.require_head(channel_id, seq).await?;
        if head.deleted {
            return Err(Error::Module(format!(
                "cannot edit a deleted message: {channel_id}/{seq}"
            )));
        }
        if head.rev >= MAX_REVISIONS - 1 {
            return Err(Error::Module(format!(
                "revision cap reached: {channel_id}/{seq}"
            )));
        }

        // head is last-write-wins under the total order; the prior head moves
        // into the immutable revision history. a stale base_rev is recorded on
        // the new head (base_rev != prior rev), never rejected: the recorded
        // base tells a reader which head the edit was written against.
        self.store(rev_key(channel_id, seq, head.rev), &head);
        let rev = head.rev + 1;
        let new_head = MessageHead {
            content_origin: authority.origin.clone(),
            blocks,
            rev,
            revision: head
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Module("message revision exhausted".into()))?,
            edited_at: Some(now),
            base_rev,
            ..head
        };
        self.store_head(channel_id, seq, &new_head)?;
        let addressed = self.dm_counterpart(channel_id, &new_head.author).await?;
        Ok((
            rev,
            Report {
                object: message_object(&new_head.message_id),
                revision: new_head.revision,
                relations: message_relations(&new_head.author, &mentions, addressed),
            },
        ))
    }

    async fn stage_delete(&mut self, channel_id: &str, seq: u64) -> Result<Report, Error> {
        require_non_empty("channel_id", channel_id)?;
        let head = self.require_head(channel_id, seq).await?;
        if head.deleted {
            return Err(Error::Module(format!(
                "message already deleted: {channel_id}/{seq}"
            )));
        }

        // clear reactions; the emoji index says which records exist.
        let emojis: BTreeSet<String> = self
            .load(&reactidx_key(channel_id, seq))
            .await?
            .unwrap_or_default();
        for emoji in &emojis {
            self.delete(react_key(channel_id, seq, emoji));
        }
        self.delete(reactidx_key(channel_id, seq));

        // tombstone: blocks cleared, skeleton (seq, thread linkage, reply
        // summary, authorship, revision history) preserved so thread integrity
        // and the sequence promise survive. the deleted message holds no
        // relation: its report withdraws every one it had.
        let tombstone = MessageHead {
            blocks: Vec::new(),
            deleted: true,
            revision: head
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Module("message revision exhausted".into()))?,
            ..head
        };
        self.store_head(channel_id, seq, &tombstone)?;
        Ok(Report {
            object: message_object(&tombstone.message_id),
            revision: tombstone.revision,
            relations: Vec::new(),
        })
    }

    /// shared reaction-op prelude: emoji + policy + target-message checks.
    async fn reaction_target(
        &self,
        authority: &Authority,
        channel_id: &str,
        seq: u64,
        emoji: &str,
    ) -> Result<(), Error> {
        require_non_empty("channel_id", channel_id)?;
        require_non_empty("emoji", emoji)?;
        if emoji.len() > MAX_EMOJI_BYTES {
            return Err(Error::Module(format!(
                "emoji too long: {} > {MAX_EMOJI_BYTES} bytes",
                emoji.len()
            )));
        }
        let channel = self.require_channel(channel_id).await?;
        self.check_authorized_post(&channel, authority).await?;
        let head = self.require_head(channel_id, seq).await?;
        if head.deleted {
            return Err(Error::Module(format!(
                "cannot react to a deleted message: {channel_id}/{seq}"
            )));
        }
        Ok(())
    }

    async fn stage_add_reaction(
        &mut self,
        authority: &Authority,
        channel_id: &str,
        seq: u64,
        emoji: &str,
    ) -> Result<Party, Error> {
        self.reaction_target(authority, channel_id, seq, emoji)
            .await?;
        let mut reactors: BTreeSet<Party> = self
            .load(&react_key(channel_id, seq, emoji))
            .await?
            .unwrap_or_default();
        let party = authority.participant(&reactors);
        if reactors.contains(&party) {
            // idempotent: a duplicate add stages NOTHING, so the qmdb op log —
            // and therefore the root — is byte-identical to a single add.
            return Ok(party);
        }
        let mut emojis: BTreeSet<String> = self
            .load(&reactidx_key(channel_id, seq))
            .await?
            .unwrap_or_default();
        if reactors.is_empty() && !emojis.contains(emoji) && emojis.len() >= MAX_REACTION_EMOJIS {
            return Err(Error::Module(format!(
                "distinct emoji cap reached: {channel_id}/{seq}"
            )));
        }
        reactors.insert(party.clone());
        if emojis.insert(emoji.to_string()) {
            self.store(reactidx_key(channel_id, seq), &emojis);
        }
        self.store_bounded(
            react_key(channel_id, seq, emoji),
            &reactors,
            MAX_MESSAGE_HEAD_BYTES,
            "reaction",
        )?;
        Ok(party)
    }

    async fn stage_remove_reaction(
        &mut self,
        authority: &Authority,
        channel_id: &str,
        seq: u64,
        emoji: &str,
    ) -> Result<Party, Error> {
        self.reaction_target(authority, channel_id, seq, emoji)
            .await?;
        let mut reactors: BTreeSet<Party> = self
            .load(&react_key(channel_id, seq, emoji))
            .await?
            .unwrap_or_default();
        let party = authority.participant(&reactors);
        if !reactors.remove(&party) {
            // exact remove: absent (emoji, party) is a deterministic no-op.
            return Ok(party);
        }
        if reactors.is_empty() {
            self.delete(react_key(channel_id, seq, emoji));
            let mut emojis: BTreeSet<String> = self
                .load(&reactidx_key(channel_id, seq))
                .await?
                .unwrap_or_default();
            emojis.remove(emoji);
            if emojis.is_empty() {
                self.delete(reactidx_key(channel_id, seq));
            } else {
                self.store(reactidx_key(channel_id, seq), &emojis);
            }
            return Ok(party);
        }
        self.store_bounded(
            react_key(channel_id, seq, emoji),
            &reactors,
            MAX_MESSAGE_HEAD_BYTES,
            "reaction",
        )?;
        Ok(party)
    }

    // ---- roster writes ----------------------------------------------------

    /// register a hook module on a channel: a standing subscription to
    /// everything posted there. any authenticated party attaches one to any
    /// channel; the caller has already checked the module is registered.
    async fn stage_register_hook(
        &mut self,
        channel_id: &str,
        module_id: String,
    ) -> Result<(), Error> {
        require_non_empty("channel_id", channel_id)?;
        require_non_empty("module_id", &module_id)?;
        let mut channel = self.require_channel(channel_id).await?;
        if channel.hooks.contains(&module_id) {
            // idempotent: registering twice stages nothing.
            return Ok(());
        }
        if channel.hooks.len() >= MAX_HOOKS_PER_CHANNEL {
            return Err(Error::Module(format!("hook cap reached: {channel_id}")));
        }
        channel.hooks.push(module_id);
        self.store_channel(&channel)
    }

    /// unregister a hook module: the one-message off switch for that
    /// automation on the channel, open to any authenticated party like
    /// registration. an absent hook stages nothing.
    async fn stage_unregister_hook(
        &mut self,
        channel_id: &str,
        module_id: &str,
    ) -> Result<(), Error> {
        require_non_empty("channel_id", channel_id)?;
        let mut channel = self.require_channel(channel_id).await?;
        let before = channel.hooks.len();
        channel.hooks.retain(|hook| hook != module_id);
        if channel.hooks.len() == before {
            return Ok(());
        }
        self.store_channel(&channel)
    }

    /// add/remove a person from the channel roster. the roster is
    /// `PostPolicy::MembersOnly`'s admission list, and any authenticated
    /// party writes it: membership is a fact about who posts, not a gate a
    /// channel's owner holds. the channel must exist; the named party is
    /// resolved after that.
    async fn stage_membership(
        &mut self,
        ctx: &dyn Ctx,
        channel_id: &str,
        member_party: Party,
        member: bool,
    ) -> Result<(), Error> {
        require_non_empty("channel_id", channel_id)?;
        self.require_channel(channel_id).await?;
        // idempotent: an unchanged membership stages nothing, so the qmdb op
        // log — and the root — is byte-identical to no write at all. the point
        // record is the policy read; the roster VIEW lives on the index tier.
        let already_member = self.is_member(channel_id, &member_party).await?;
        if already_member == member {
            return Ok(());
        }
        if member {
            self.validate_member(ctx, &member_party).await?;
            self.store(member_key(channel_id, &member_party), &true);
        } else {
            self.delete(member_key(channel_id, &member_party));
        }
        Ok(())
    }

    /// join (or start) the channel's huddle. only external users may — the
    /// roster is a room of people, so module/system origins are rejected —
    /// and members-only channels gate exactly like posting. `node_proof` must
    /// verify as `node`'s own signature over this join (proof of possession —
    /// see [`interface::huddle_join_preimage`]), refused with
    /// `huddle_node_proof_invalid` otherwise. re-joining with the same node
    /// key stages nothing (idempotent, byte-identical op log).
    async fn stage_join_huddle(
        &mut self,
        authority: &Authority,
        channel_id: &str,
        node: Vec<u8>,
        node_proof: Vec<u8>,
        now: u64,
    ) -> Result<Party, Error> {
        let party = authority.party.clone();
        require_non_empty("channel_id", channel_id)?;
        if !party.is_person() {
            return Err(Error::Module("only people may join a huddle".into()));
        }
        if node.len() != HUDDLE_NODE_KEY_BYTES {
            return Err(Error::Module(format!(
                "huddle node key must be {HUDDLE_NODE_KEY_BYTES} bytes, got {}",
                node.len()
            )));
        }
        let (namespace, preimage) = match &authority.origin {
            Origin::External(key) => (HUDDLE_JOIN_NS, huddle_join_preimage(channel_id, key)),
            Origin::Program(account) => (
                PROGRAM_HUDDLE_JOIN_NS,
                program_huddle_join_preimage(channel_id, *account),
            ),
            Origin::Module(_) | Origin::System => {
                return Err(Error::Module("only people may join a huddle".into()));
            }
        };
        if !keyscheme::KeyScheme::Ed25519.verify(&node, namespace, &preimage, &node_proof) {
            return Err(Error::Module("huddle_node_proof_invalid".into()));
        }
        let mut channel = self.require_channel(channel_id).await?;
        let party = authority.participant(channel.huddle.iter().map(|member| &member.party));
        self.check_authorized_post(&channel, authority).await?;
        if let Some(existing) = channel.huddle.iter_mut().find(|m| m.party == party) {
            if existing.node == node {
                return Ok(party);
            }
            existing.node = node;
        } else {
            if channel.huddle.len() >= MAX_HUDDLE_MEMBERS {
                return Err(Error::Module(format!("huddle is full: {channel_id}")));
            }
            channel.huddle.push(HuddleMember {
                party: party.clone(),
                node,
                joined_at: now,
            });
        }
        self.store_channel(&channel)?;
        Ok(party)
    }

    /// leave the channel's huddle. absent participation is a deterministic
    /// no-op; the last leaver empties the roster (= the huddle ends).
    async fn stage_leave_huddle(
        &mut self,
        authority: &Authority,
        channel_id: &str,
    ) -> Result<Party, Error> {
        let party = &authority.party;
        require_non_empty("channel_id", channel_id)?;
        if !party.is_person() {
            return Err(Error::Module("only people may leave a huddle".into()));
        }
        let mut channel = self.require_channel(channel_id).await?;
        let party = authority.participant(channel.huddle.iter().map(|member| &member.party));
        let before = channel.huddle.len();
        channel.huddle.retain(|m| m.party != party);
        if channel.huddle.len() == before {
            return Ok(party);
        }
        self.store_channel(&channel)?;
        Ok(party)
    }

    /// evict `target` from the channel's huddle (staleness cleanup — see
    /// `ChatMsg::SweepHuddle`). a person naming themself is a leave in
    /// disguise; naming anyone else evicts them, by any person: `HuddleMember`
    /// carries only `joined_at`, set once at join and never refreshed on
    /// liveness, so the module holds no call-presence signal a staleness
    /// rule could read, and the room's people are its only cleanup. absent
    /// target = no-op either way.
    async fn stage_sweep_huddle(
        &mut self,
        authority: &Authority,
        channel_id: &str,
        target: &Party,
    ) -> Result<Party, Error> {
        let party = &authority.party;
        require_non_empty("channel_id", channel_id)?;
        if !party.is_person() {
            return Err(Error::Module("only people may sweep a huddle".into()));
        }
        if target == party {
            return self.stage_leave_huddle(authority, channel_id).await;
        }
        let mut channel = self.require_channel(channel_id).await?;
        let before = channel.huddle.len();
        channel.huddle.retain(|m| m.party != *target);
        if channel.huddle.len() == before {
            return Ok(target.clone());
        }
        self.store_channel(&channel)?;
        Ok(target.clone())
    }

    /// hand one report to the attribution plane in this unit — the write and
    /// its attribution commit or abort together. a host wiring no plane
    /// reports nothing.
    fn report(&self, ctx: &mut dyn Ctx, actor: &Party, report: Report) {
        let Some(attribution) = &self.attribution else {
            return;
        };
        ctx.emit_msg(Msg {
            target: attribution.clone(),
            payload: attribution_encode_msg(&AttributionMsg::Attribute {
                object: report.object,
                revision: report.revision,
                actor: actor_of(actor),
                relations: report.relations,
                transfers: Vec::new(),
            }),
        });
    }

    // ---- dispatch reads --------------------------------------------------
    // the three reads other modules' execute() paths consume. everything a
    // human lists, scrolls, or searches is the index guest's job (index.rs).

    /// point-lookup one page of message views for computed sequences. the
    /// sequence space is gap-free (P3), so a missing head is a store bug.
    async fn views(
        &self,
        channel_id: &str,
        seqs: impl Iterator<Item = u64>,
    ) -> Result<Vec<MessageView>, Error> {
        let mut views = Vec::new();
        for seq in seqs {
            let head = self.require_head(channel_id, seq).await.map_err(|_| {
                Error::Module(format!("missing message record: {channel_id}/{seq}"))
            })?;
            views.push(MessageView {
                channel_id: channel_id.to_string(),
                seq,
                head,
            });
        }
        Ok(views)
    }

    async fn messages_range(
        &self,
        channel_id: &str,
        from_seq: u64,
        limit: u64,
    ) -> Result<Vec<MessageView>, Error> {
        let channel = self.require_channel(channel_id).await?;
        let limit = clamp_limit(limit);
        let from = from_seq.max(1);
        if limit == 0 || from > channel.head_seq {
            return Ok(Vec::new());
        }
        let to = channel.head_seq.min(from.saturating_add(limit - 1));
        self.views(channel_id, from..=to).await
    }

    async fn message_by_id(&self, message_id: &str) -> Result<Option<MessageView>, Error> {
        let Some((channel_id, seq)) = self.load::<(String, u64)>(&msgid_key(message_id)).await?
        else {
            return Ok(None);
        };
        let head = self.require_head(&channel_id, seq).await?;
        Ok(Some(MessageView {
            channel_id,
            seq,
            head,
        }))
    }
}

impl Chat {
    async fn execute_op(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let now = ctx.env().consensus_time;
        // every write op acts as an authenticated party, even ops that do not
        // store one — the empty demo-default external origin never passes.
        let origin = ctx.env().origin.clone();
        let party = self.party_of_origin(&*ctx, &origin).await?;
        let authority = Authority {
            party: party.clone(),
            origin,
        };
        ctx.set_assigned(encode_assigned(&ChatAssigned::Actor {
            actor: party.clone(),
        }));
        match decode_msg(&msg.payload).map_err(Error::Module)? {
            ChatMsg::CreateChannel {
                channel_id,
                name,
                post_policy,
            } => {
                let report = self
                    .stage_channel(&party, channel_id, name, post_policy, false, now)
                    .await?;
                self.report(ctx, &party, report);
                Ok(())
            }
            ChatMsg::CreateVoiceChannel { channel_id, name } => {
                let report = self
                    .stage_channel(&party, channel_id, name, PostPolicy::Open, true, now)
                    .await?;
                self.report(ctx, &party, report);
                Ok(())
            }
            ChatMsg::CreateDmChannel { counterpart, name } => {
                let (channel_id, report) = self
                    .stage_dm_channel(&*ctx, &party, counterpart, name, now)
                    .await?;
                ctx.set_output(sdk::wire::encode(&channel_id));
                ctx.set_assigned(encode_assigned(&ChatAssigned::DmChannel {
                    channel_id,
                    actor: party.clone(),
                }));
                self.report(ctx, &party, report);
                Ok(())
            }
            ChatMsg::RenameChannel { channel_id, name } => {
                if let Some(report) = self.stage_rename(&authority, &channel_id, name).await? {
                    self.report(ctx, &party, report);
                }
                Ok(())
            }
            ChatMsg::SetChannelArchived {
                channel_id,
                archived,
            } => {
                if let Some(report) = self
                    .stage_set_archived(&authority, &channel_id, archived)
                    .await?
                {
                    self.report(ctx, &party, report);
                }
                Ok(())
            }
            ChatMsg::PostMessage {
                channel_id,
                message_id,
                mut blocks,
                thread,
            } => {
                let mentions = self.resolve_mentions(&*ctx, &mut blocks).await?;
                let posted = self
                    .stage_message(
                        &authority,
                        &channel_id,
                        message_id.clone(),
                        MessageContent {
                            blocks,
                            mentions: mentions.accounts.clone(),
                        },
                        thread,
                        now,
                    )
                    .await?;
                ctx.set_assigned(encode_assigned(&ChatAssigned::Posted {
                    seq: posted.seq,
                    actor: party.clone(),
                    key_mentions: mentions.key_mentions,
                }));
                ctx.set_output(sdk::wire::encode(&serde_json::json!({ "channel_id": channel_id, "message_id": message_id, "seq": posted.seq })));
                // one follow-up per registered hook, drained in this block —
                // the message and every notification commit (or abort) as one
                // atomic unit (P2). chat stays agent-agnostic: any subscriber
                // module decodes the ChatEvent payload.
                for hook in posted.hooks {
                    ctx.emit_msg(Msg {
                        target: hook,
                        payload: encode_event(&ChatEvent::MessagePosted {
                            channel_id: channel_id.clone(),
                            seq: posted.seq,
                            thread_root: posted.thread_root,
                            author: party.clone(),
                            mentions: mentions.accounts.clone(),
                        }),
                    });
                }
                self.report(ctx, &party, posted.report);
                Ok(())
            }
            ChatMsg::EditMessage {
                channel_id,
                seq,
                mut blocks,
                base_rev,
            } => {
                let mentions = self.resolve_mentions(&*ctx, &mut blocks).await?;
                let (rev, report) = self
                    .stage_edit(
                        &authority,
                        &channel_id,
                        seq,
                        MessageContent {
                            blocks,
                            mentions: mentions.accounts,
                        },
                        base_rev,
                        now,
                    )
                    .await?;
                ctx.set_assigned(encode_assigned(&ChatAssigned::Edited {
                    rev,
                    actor: party.clone(),
                    key_mentions: mentions.key_mentions,
                }));
                self.report(ctx, &party, report);
                Ok(())
            }
            ChatMsg::DeleteMessage { channel_id, seq } => {
                let report = self.stage_delete(&channel_id, seq).await?;
                self.report(ctx, &party, report);
                Ok(())
            }
            ChatMsg::AddReaction {
                channel_id,
                seq,
                emoji,
            } => {
                let participant = self
                    .stage_add_reaction(&authority, &channel_id, seq, &emoji)
                    .await?;
                ctx.set_assigned(encode_assigned(&ChatAssigned::Participant {
                    actor: party.clone(),
                    participant,
                }));
                Ok(())
            }
            ChatMsg::RemoveReaction {
                channel_id,
                seq,
                emoji,
            } => {
                let participant = self
                    .stage_remove_reaction(&authority, &channel_id, seq, &emoji)
                    .await?;
                ctx.set_assigned(encode_assigned(&ChatAssigned::Participant {
                    actor: party.clone(),
                    participant,
                }));
                Ok(())
            }
            ChatMsg::RegisterHook {
                channel_id,
                module_id,
            } => {
                // the target must be a registered module other than chat
                // itself, or every later post would poison the block; any
                // authenticated party may attach it.
                if module_id == self.id {
                    return Err(Error::Module("chat cannot hook itself".into()));
                }
                if ctx.module_root(&module_id).is_none() {
                    return Err(Error::Module(format!("unknown hook module: {module_id}")));
                }
                self.stage_register_hook(&channel_id, module_id).await
            }
            ChatMsg::UnregisterHook {
                channel_id,
                module_id,
            } => self.stage_unregister_hook(&channel_id, &module_id).await,
            ChatMsg::SetMembership {
                channel_id,
                party: member_party,
                member,
            } => {
                self.stage_membership(&*ctx, &channel_id, member_party, member)
                    .await
            }
            ChatMsg::JoinHuddle {
                channel_id,
                node,
                node_proof,
            } => {
                let participant = self
                    .stage_join_huddle(&authority, &channel_id, node, node_proof, now)
                    .await?;
                ctx.set_assigned(encode_assigned(&ChatAssigned::Participant {
                    actor: party.clone(),
                    participant,
                }));
                Ok(())
            }
            ChatMsg::LeaveHuddle { channel_id } => {
                let participant = self.stage_leave_huddle(&authority, &channel_id).await?;
                ctx.set_assigned(encode_assigned(&ChatAssigned::Participant {
                    actor: party.clone(),
                    participant,
                }));
                Ok(())
            }
            ChatMsg::SweepHuddle {
                channel_id,
                party: target,
            } => {
                let participant = self
                    .stage_sweep_huddle(&authority, &channel_id, &target)
                    .await?;
                ctx.set_assigned(encode_assigned(&ChatAssigned::Participant {
                    actor: party.clone(),
                    participant,
                }));
                Ok(())
            }
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Chat {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the store's merkle root over all committed records, verbatim — the
    /// staged overlay is invisible here until `commit_block`.
    fn root(&self) -> StateRoot {
        self.staged.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }

    /// the network state-sync serve lane: answers the shared qmdb wire requests
    /// (historical proof-carrying op ranges) from committed state. read-only;
    /// the joiner's sync engine merkle-verifies every batch.
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let checkpoint = self.staged.checkpoint();
        match self.execute_op(ctx, msg).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.staged.restore(checkpoint);
                Err(error)
            }
        }
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(Error::Module)? {
            ChatQuery::Channel { channel_id } => Ok(encode_reply(&ChatReply::Channel(
                self.channel(&channel_id).await?,
            ))),
            ChatQuery::MessagesRange {
                channel_id,
                from_seq,
                limit,
            } => Ok(encode_reply(&ChatReply::Messages(
                self.messages_range(&channel_id, from_seq, limit).await?,
            ))),
            ChatQuery::Message { message_id } => Ok(encode_reply(&ChatReply::Message(
                self.message_by_id(&message_id).await?,
            ))),
            ChatQuery::Access { channel_id, party } => Ok(encode_reply(&ChatReply::Access(
                self.channel_access(&channel_id, &party).await?,
            ))),
        }
    }

    /// publish the block's staged writes in ONE store batch. no-op (and no
    /// root movement) if nothing was staged. BTreeMap iteration keeps the
    /// write order deterministic across validators, and a staged `None` ships
    /// as a delete of the hashed key.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}
