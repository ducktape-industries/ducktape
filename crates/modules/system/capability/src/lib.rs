//! the qmdb-backed network-wide registry of node host capabilities.
//!
//! a "capability" is an open-set string tag ("codex", "claude", ...) naming
//! something a node's HOST can execute for the network — the consensus-side
//! half of the provider seam: this module replicates *who provides what*;
//! actually spawning a provider is host code and never happens here (no I/O,
//! no provider-specific logic). every node holds an identical view at every
//! height, which is exactly the property work assignment needs (all nodes
//! must agree deterministically on who could serve a job).
//!
//! announcements are declarative, self-scoped, and truthful by construction:
//! [`CapabilityMsg::Announce`] replaces the SUBMITTER's full tag set — the
//! announced identity is the verified external submit origin, never payload
//! data, so a node can only speak for itself. an empty set removes the node.
//! when constructed with a valset id, announcements are additionally gated to
//! current validators UNION residents:
//! a joined-but-not-promoted node provides real executors too, and its
//! announce is what lets dispatch route work to it. without a valset (the
//! single-node daemon) any external key may self-announce. the gate is not
//! announce-time-only: [`Module::query_with`] re-checks standing on every
//! `Providers` / `CapableProviders` read, so a node that leaves, is revoked,
//! or drops out of the resident window stops being returned even though its
//! stale record is still in the roster — nothing needs to delete the record
//! for the node to stop being handed work. (liveness of a standing node —
//! whether it is actually up — is a separate concern this module does not
//! address.)
//!
//! an `Announce` may additionally carry `resources`: announced numeric
//! capacity per open-set dimension ("cores" -> 8, "mem_gb" -> 32), riding the
//! same declarative replace as tags. capacity with nothing to execute is
//! meaningless, so resources without at least one tag is rejected; a
//! tags-only node stays valid (capacity is optional — a claim is a claim)
//! but never satisfies a demands-carrying query — absent is never infinite.
//! [`CapabilityQuery::CapableProviders`] is the read work assignment filters
//! on: providers of a capability whose announced resources cover every
//! demanded dimension.
//!
//! ## capability classes
//!
//! the registry additionally routes capability CLASSES — namespace tokens
//! (`agent`, `ai`, ...) a MODULE claims via [`CapabilityMsg::ClaimClass`].
//! the wire form of a classed capability is `<class>:<rest>` and dispatch
//! addresses classed work as `<node_id>/<class>:<rest>` (see
//! [`parse_classed_address`]); the class map (class -> claimant module) is
//! the primary router for that space. the claimant is the verified MODULE
//! origin — a class is claimed by the module that serves it, so External and
//! System origins reject. first claim wins deterministically; a re-claim by
//! the owner is an idempotent no-op. there is deliberately NO unclaim op:
//! dropping a claim would dangle every `<class>:` route already minted, so
//! claims are permanent.
//!
//! ## State model
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: the HOST constructs
//! the concrete store (qmdb today — `statesync::qmdb::QmdbStore`) and hands
//! it to [`CapabilityRegistry::new`], so this crate never names a storage
//! crate. one logical record per announced node (`node\0{key}`, borsh) and
//! per claimed class (`class\0{name}`), plus the two rosters the scan reads
//! walk — the sorted node list (`nodes`, bounded by [`MAX_ANNOUNCED_NODES`])
//! and the sorted class list (`classes`, bounded by [`MAX_CLASSES`]). the
//! provider scans (`Providers` / `CapableProviders` / `All`) are
//! DISPATCH-CONSUMED (saga's assignment filters on them at execute), so they
//! stay canonical behind the capped roster; `Node` / `Resources` /
//! `ResolveClass` are point reads.
//!
//! writes are staged during a block and flushed to the store in one batch at
//! `commit_block`; the module root IS the store's merkle root. sync belongs
//! to the store, not this module: a joiner rebuilds the concrete store from
//! a peer (`QmdbStore::sync_from`) and wraps a fresh registry around it.
//!
//! oversized values never reach the store (the poison-value lesson): a node
//! entry is bounded by construction ([`MAX_CAPABILITIES`] tags of
//! `MAX_TAG_LEN` + [`MAX_RESOURCE_DIMS`] dimensions — `validate_tags` /
//! `validate_resources` gate every announce), a class record by
//! `MAX_CLASS_LEN`, and both rosters are byte-gated on top of their count
//! caps.

mod valset_contract;
mod wire;

// the wire surface belongs to this module. Keep its codec at the crate root
// so existing host consumers use the same bytes without an SDK wire crate.
pub use wire::*;

use std::collections::{BTreeMap, BTreeSet};

use borsh::{BorshDeserialize, BorshSerialize};
use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore,
    StateRoot, StateSyncHandle,
};
use valset_contract::members_and_residents;

/// most tags a single node may announce. a bound, not a schema: it exists so
/// one announcement cannot bloat replicated state, while staying far above
/// any real host's executor count.
const MAX_CAPABILITIES: usize = 64;

/// announced nodes retained at once (the roster count cap). announcements are
/// valset ∪ resident gated in production, so this sits far above any real
/// network's node count; announcing past it refuses loudly at execute.
pub const MAX_ANNOUNCED_NODES: usize = 1024;
/// serialized node-roster byte bound — the backstop on top of the count cap
/// (node keys are opaque origin bytes, so the count alone does not bound the
/// serialized form).
pub const MAX_NODE_ROSTER_RECORD_BYTES: usize = 512 * 1024;
/// classes retained over the network's life (claims are permanent).
pub const MAX_CLASSES: usize = 1024;
/// serialized class-roster byte bound (class names are ≤ `MAX_CLASS_LEN`, so
/// this is generous by construction — kept as the uniform poison backstop).
pub const MAX_CLASS_ROSTER_RECORD_BYTES: usize = 512 * 1024;

/// per-node record key: prefix + 0 + node key (the single-component shape
/// chat uses). safe because every key literal below is fixed and none is
/// another followed by a 0 byte.
fn node_key(node: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + 1 + node.len());
    key.extend_from_slice(b"node");
    key.push(0);
    key.extend_from_slice(node);
    key
}

/// per-class record key: prefix + 0 + class name. valued by the claimant
/// module id.
fn class_key(class: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(5 + 1 + class.len());
    key.extend_from_slice(b"class");
    key.push(0);
    key.extend_from_slice(class.as_bytes());
    key
}

/// the node roster's whole key. collides with no `node\0...` / `class\0...`
/// key.
const NODE_ROSTER_KEY: &[u8] = b"nodes";

/// the class roster's whole key.
const CLASS_ROSTER_KEY: &[u8] = b"classes";

/// one node's registry entry: the tag set it announced plus the numeric
/// capacity it announced per dimension. an entry is stored ONLY with
/// non-empty tags (empty means absent — an empty announce deletes the
/// record), and `execute` rejects resources without at least one tag, so
/// every stored entry with non-empty resources also has non-empty tags.
/// stored verbatim — borsh writes the set and map length-prefixed in key
/// order, so one entry has exactly one encoding.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
struct NodeEntry {
    tags: BTreeSet<String>,
    resources: BTreeMap<String, u64>,
}

pub struct CapabilityRegistry {
    id: ModuleId,
    /// the valset module consulted to gate announcements to current members;
    /// `None` runs ungated (the single-node daemon carries no valset).
    valset_id: Option<ModuleId>,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`). store key
    /// is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl CapabilityRegistry {
    /// wrap the host-constructed store under module identity `id`.
    pub fn new(
        id: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
        valset_id: Option<ModuleId>,
    ) -> Self {
        Self {
            id: id.into(),
            valset_id,
            staged: StagedStore::new(store),
        }
    }

    // ---- staged-over-committed reads ----------------------------------------

    async fn load<T>(&self, key: &[u8]) -> Result<Option<T>, Error>
    where
        T: BorshDeserialize,
    {
        match self.staged.get(key).await? {
            Some(bytes) => Ok(Some(
                borsh::from_slice(&bytes).map_err(|e| Error::module("codec", e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// stage a value whose serialized size is bounded by construction (a node
    /// entry, a class claim) — see the module doc's poison-value paragraph.
    /// the rosters go through [`Self::store_bounded`].
    fn store<T>(&mut self, key: Vec<u8>, value: &T)
    where
        T: BorshSerialize,
    {
        self.staged.stage(
            key,
            borsh::to_vec(value).expect("capability value is serializable"),
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
        T: BorshSerialize,
    {
        let bytes = borsh::to_vec(value).expect("capability value is serializable");
        if bytes.len() > cap {
            return Err(Error::module(
                "record_too_large",
                format!("{what} record too large: {} > {cap} bytes", bytes.len()),
            ));
        }
        self.staged.stage(key, bytes);
        Ok(())
    }

    async fn entry(&self, node: &[u8]) -> Result<Option<NodeEntry>, Error> {
        self.load(&node_key(node)).await
    }

    /// a node the roster points at. a rostered key without its record is a
    /// store bug — loud, never skipped.
    async fn rostered_entry(&self, node: &[u8]) -> Result<NodeEntry, Error> {
        self.entry(node)
            .await?
            .ok_or_else(|| Error::module("missing_node_record", "missing node record"))
    }

    /// An empty roster has no standing to check. This also lets a network
    /// without valset read its empty provider list while announcements stay gated.
    async fn standing_nodes(&self, ctx: &dyn Ctx, valset_id: &str) -> Result<Vec<Vec<u8>>, Error> {
        let nodes = self.node_roster().await?;
        let empty_roster = nodes.is_empty();
        if empty_roster {
            return Ok(nodes);
        }
        let standing = members_and_residents(ctx, valset_id).await?;
        Ok(nodes
            .into_iter()
            .filter(|node| standing.contains(node))
            .collect())
    }

    /// the node roster — every announced node key, sorted. record and roster
    /// are staged (and commit or abort) together, so membership in one is
    /// membership in both.
    async fn node_roster(&self) -> Result<Vec<Vec<u8>>, Error> {
        Ok(self.load(NODE_ROSTER_KEY).await?.unwrap_or_default())
    }

    /// the class roster — every claimed class name, sorted.
    async fn class_roster(&self) -> Result<Vec<String>, Error> {
        Ok(self.load(CLASS_ROSTER_KEY).await?.unwrap_or_default())
    }

    /// the module owning `class` in the staged-over-committed view — the
    /// first-claim-wins check reads THIS, so a claim staged earlier in the
    /// block already blocks a rival in the same block.
    async fn class_owner(&self, class: &str) -> Result<Option<ModuleId>, Error> {
        self.load(&class_key(class)).await
    }

    /// validate and canonicalize an announced tag list. duplicates collapse
    /// (set semantics — announcing ["codex", "codex"] is not an error), shape
    /// violations reject deterministically: every validator sees the same
    /// bytes, so every validator rejects identically.
    fn validate_tags(tags: Vec<String>) -> Result<BTreeSet<String>, Error> {
        if tags.len() > MAX_CAPABILITIES {
            return Err(Error::module(
                "capability_cap",
                format!(
                    "too many capabilities: {} exceeds the {MAX_CAPABILITIES} cap",
                    tags.len()
                ),
            ));
        }
        let mut set = BTreeSet::new();
        for tag in tags {
            validate_tag(&tag).map_err(|e| Error::module("bad_capability_tag", e))?;
            set.insert(tag);
        }
        Ok(set)
    }

    // ---- the op handlers ------------------------------------------------------

    /// declarative replace of the SUBMITTER's entry: non-empty tags upsert the
    /// record (claiming a roster slot on first announce), empty tags delete it
    /// (and free the slot). removing a node that never announced is a no-op
    /// that stages nothing.
    async fn handle_announce(
        &mut self,
        ctx: &mut dyn Ctx,
        capabilities: Vec<String>,
        resources: BTreeMap<String, u64>,
    ) -> Result<(), Error> {
        // identity comes from the verified submit origin, never the payload —
        // a node can only announce for itself, which keeps the registry
        // truthful by construction. module/system origins have no host of
        // their own to speak for, and an empty key is a malformed origin.
        let node = match &ctx.env().origin {
            Origin::External(key) if key.is_empty() => {
                return Err(Error::module(
                    "empty_origin_key",
                    "external origin key is empty",
                ));
            }
            Origin::External(key) => key.clone(),
            other => {
                return Err(Error::module(
                    "external_origin_required",
                    format!(
                        "capability announcements require an external submitter, got {other:?}"
                    ),
                ));
            }
        };
        if let Some(valset_id) = self.valset_id.clone() {
            // member-gated: validators UNION residents populate the registry,
            // so lookups resolve to known peers — including a joined node
            // that has not been promoted yet.
            if !members_and_residents(ctx, &valset_id)
                .await?
                .contains(&node)
            {
                return Err(Error::module(
                    "no_standing",
                    "capability announcer holds no current standing (validator or resident)",
                ));
            }
        }
        let tags = Self::validate_tags(capabilities)?;
        validate_resources(&resources).map_err(|e| Error::module("bad_resources", e))?;
        if tags.is_empty() && !resources.is_empty() {
            return Err(Error::module(
                "resources_without_tags",
                "resources without capabilities (announce at least one tag)",
            ));
        }

        let current = self.entry(&node).await?;
        let announced = current.is_some();
        if tags.is_empty() {
            // removal: drop the record and free its roster slot. a node that
            // never announced has nothing to remove — stage nothing.
            if !announced {
                return Ok(());
            }
            let mut roster = self.node_roster().await?;
            if let Ok(position) = roster.binary_search(&node) {
                roster.remove(position);
            }
            if roster.is_empty() {
                self.staged.delete(NODE_ROSTER_KEY.to_vec());
            } else {
                self.store(NODE_ROSTER_KEY.to_vec(), &roster);
            }
            self.staged.delete(node_key(&node));
            return Ok(());
        }

        let entry = NodeEntry { tags, resources };
        // re-announcing the CURRENT set is an idempotent no-op that stages
        // nothing — the root must hold (a byte-identical overwrite would
        // still be a committed store op).
        if current.as_ref() == Some(&entry) {
            return Ok(());
        }
        if !announced {
            let mut roster = self.node_roster().await?;
            let Err(position) = roster.binary_search(&node) else {
                return Err(Error::module(
                    "roster_corrupt",
                    "node roster carries a key with no record",
                ));
            };
            if roster.len() >= MAX_ANNOUNCED_NODES {
                return Err(Error::module(
                    "node_cap",
                    format!("announced-node cap reached ({MAX_ANNOUNCED_NODES})"),
                ));
            }
            roster.insert(position, node.clone());
            self.store_bounded(
                NODE_ROSTER_KEY.to_vec(),
                &roster,
                MAX_NODE_ROSTER_RECORD_BYTES,
                "node roster",
            )?;
        }
        // bounded by construction: validate_tags + validate_resources gated it.
        self.store(node_key(&node), &entry);
        Ok(())
    }

    /// claim `class` for the verified MODULE origin. first claim wins (read
    /// against the staged-over-committed view, so a claim staged earlier in
    /// the block already binds); a re-claim by the owner is an idempotent
    /// no-op that stages nothing.
    async fn handle_claim_class(&mut self, ctx: &mut dyn Ctx, class: String) -> Result<(), Error> {
        // a class is claimed by the module that serves it: the claimant is
        // the verified MODULE origin, never payload data. external submitters
        // and the system have no module to route classed work to, so both
        // reject.
        let module = match &ctx.env().origin {
            Origin::Module(id) => id.clone(),
            other => {
                return Err(Error::module(
                    "module_origin_required",
                    format!(
                        "a class is claimed by the module that serves it \
                         (module origin required), got {other:?}"
                    ),
                ));
            }
        };
        validate_class(&class).map_err(|e| Error::module("bad_class", e))?;
        match self.class_owner(&class).await? {
            Some(owner) if owner != module => Err(Error::module(
                "class_already_claimed",
                format!(
                    "class {class:?} is already claimed by module {owner:?} \
                     (first claim wins)"
                ),
            )),
            // a re-claim by the owning module is an idempotent no-op: nothing
            // is staged, so the root cannot move.
            Some(_) => Ok(()),
            None => {
                let mut roster = self.class_roster().await?;
                let Err(position) = roster.binary_search(&class) else {
                    return Err(Error::module(
                        "roster_corrupt",
                        "class roster carries a name with no record",
                    ));
                };
                if roster.len() >= MAX_CLASSES {
                    return Err(Error::module(
                        "class_cap",
                        format!("class cap reached ({MAX_CLASSES})"),
                    ));
                }
                roster.insert(position, class.clone());
                self.store_bounded(
                    CLASS_ROSTER_KEY.to_vec(),
                    &roster,
                    MAX_CLASS_ROSTER_RECORD_BYTES,
                    "class roster",
                )?;
                // bounded by construction: a validated class name + module id.
                self.store(class_key(&class), &module);
                Ok(())
            }
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for CapabilityRegistry {
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
        match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
            CapabilityMsg::Announce {
                capabilities,
                resources,
            } => self.handle_announce(ctx, capabilities, resources).await,
            CapabilityMsg::ClaimClass { class } => self.handle_claim_class(ctx, class).await,
        }
    }

    /// read projection with access to host-routed reads of sibling modules:
    /// the standing re-check the announce-time gate alone cannot provide.
    /// `Providers` / `CapableProviders` are DISPATCH-CONSUMED (saga's
    /// `assignment_pool` feeds them straight into `pick_assignee`), so a node
    /// that announced while gated and later lost standing — left the valset,
    /// was revoked, or a resident promotion window closed — must stop being
    /// handed work even though its stale record is still in the roster.
    /// gated once per query (not once per roster entry), then intersected
    /// with the tag/resource filter every read already applies; an ungated
    /// registry (no valset — the single-node daemon) falls through to
    /// [`Module::query`] unchanged.
    async fn query_with(&self, ctx: &dyn Ctx, req: &[u8]) -> Result<Vec<u8>, Error> {
        let Some(valset_id) = self.valset_id.clone() else {
            return self.query(req).await;
        };
        Ok(
            match decode_query(req).map_err(|e| Error::module("codec", e))? {
                CapabilityQuery::Providers { capability } => {
                    let mut providers = Vec::new();
                    for node in self.standing_nodes(ctx, &valset_id).await? {
                        let has_capability =
                            self.rostered_entry(&node).await?.tags.contains(&capability);
                        if has_capability {
                            providers.push(node);
                        }
                    }
                    encode_reply(&CapabilityReply::Providers(providers))
                }
                CapabilityQuery::CapableProviders {
                    capability,
                    demands,
                } => {
                    let mut providers = Vec::new();
                    for node in self.standing_nodes(ctx, &valset_id).await? {
                        let entry = self.rostered_entry(&node).await?;
                        let covers = demands
                            .iter()
                            .all(|(k, v)| entry.resources.get(k).is_some_and(|have| have >= v));
                        if entry.tags.contains(&capability) && covers {
                            providers.push(node);
                        }
                    }
                    encode_reply(&CapabilityReply::Providers(providers))
                }
                _ => return self.query(req).await,
            },
        )
    }

    /// read projection — committed plus this block's staged changes (the
    /// staged-over-committed store view). the provider scans walk the roster
    /// by derived key (≤ [`MAX_ANNOUNCED_NODES`] point reads). standing is not
    /// re-checked here — see [`Self::query_with`], the ctx-routed lane every
    /// real caller (saga's `assignment_pool` included) goes through; this
    /// stays the ungated (no-valset) fallback plus a plain point-read path
    /// for the same-crate tests.
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(
            match decode_query(req).map_err(|e| Error::module("codec", e))? {
                CapabilityQuery::Providers { capability } => {
                    let mut providers = Vec::new();
                    for node in self.node_roster().await? {
                        if self.rostered_entry(&node).await?.tags.contains(&capability) {
                            providers.push(node);
                        }
                    }
                    encode_reply(&CapabilityReply::Providers(providers))
                }
                CapabilityQuery::Node { node } => {
                    let tags = self
                        .entry(&node)
                        .await?
                        .map(|e| e.tags.into_iter().collect())
                        .unwrap_or_default();
                    encode_reply(&CapabilityReply::Node(tags))
                }
                CapabilityQuery::CapableProviders {
                    capability,
                    demands,
                } => {
                    let mut providers = Vec::new();
                    for node in self.node_roster().await? {
                        let entry = self.rostered_entry(&node).await?;
                        let covers = demands
                            .iter()
                            .all(|(k, v)| entry.resources.get(k).is_some_and(|have| have >= v));
                        if entry.tags.contains(&capability) && covers {
                            providers.push(node);
                        }
                    }
                    encode_reply(&CapabilityReply::Providers(providers))
                }
                CapabilityQuery::Resources { node } => {
                    let resources = self
                        .entry(&node)
                        .await?
                        .map(|e| e.resources)
                        .unwrap_or_default();
                    encode_reply(&CapabilityReply::Resources(resources))
                }
                CapabilityQuery::All => {
                    let mut all = Vec::new();
                    for node in self.node_roster().await? {
                        let entry = self.rostered_entry(&node).await?;
                        all.push((node, entry.tags.into_iter().collect()));
                    }
                    encode_reply(&CapabilityReply::All(all))
                }
                CapabilityQuery::ResolveClass { class } => encode_reply(
                    &CapabilityReply::ClassOwner(self.class_owner(&class).await?),
                ),
                CapabilityQuery::Classes => {
                    let mut classes = Vec::new();
                    for class in self.class_roster().await? {
                        let owner = self.class_owner(&class).await?.ok_or_else(|| {
                            Error::module(
                                "roster_corrupt",
                                "class roster carries a name with no record",
                            )
                        })?;
                        classes.push((class, owner));
                    }
                    encode_reply(&CapabilityReply::Classes(classes))
                }
            },
        )
    }

    /// publish the block's staged writes in ONE store batch. no-op (and no
    /// root movement) if nothing was staged.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::valset_contract::{
        ValsetQuery, ValsetReply, decode_query as valset_decode_query,
        encode_reply as valset_encode_reply,
    };
    use crate::{MAX_CLASS_LEN, MAX_TAG_LEN, encode_msg, encode_query};

    use sdk_testkit::{MemStore, TestCtx};

    /// a valset-query responder over an optional member/resident set — answers
    /// BOTH Validators and Residents so the member gate is testable for either
    /// standing (mirrors identity's test stub).
    fn valset_reads(
        members: Option<Vec<Vec<u8>>>,
        residents: Option<Vec<Vec<u8>>>,
    ) -> impl FnMut(&[u8]) -> Result<Vec<u8>, Error> {
        move |req| {
            let q = valset_decode_query(req).map_err(|e| Error::module("codec", e))?;
            match (q, &members, &residents) {
                (ValsetQuery::Validators, Some(m), _) => {
                    Ok(valset_encode_reply(&ValsetReply::Validators(m.clone())))
                }
                (ValsetQuery::Residents, _, Some(o)) => {
                    Ok(valset_encode_reply(&ValsetReply::Residents(o.clone())))
                }
                _ => Err(Error::QueryUnsupported),
            }
        }
    }

    fn ctx_with(
        origin: sdk::Origin,
        members: Option<Vec<Vec<u8>>>,
        residents: Option<Vec<Vec<u8>>>,
    ) -> TestCtx {
        TestCtx::with_env(sdk::Env {
            height: 0,
            consensus_time: 0,
            origin,
            me: "capability".into(),
            cause: sdk::Cause::Direct,
        })
        .on_query("valset", valset_reads(members, residents))
    }

    fn ctx_origin(origin: sdk::Origin) -> TestCtx {
        ctx_with(origin, None, None)
    }
    fn ctx_external(key: &[u8]) -> TestCtx {
        ctx_origin(sdk::Origin::External(key.to_vec()))
    }
    fn ctx_gated(key: &[u8], validators: Vec<Vec<u8>>, residents: Vec<Vec<u8>>) -> TestCtx {
        ctx_with(
            sdk::Origin::External(key.to_vec()),
            Some(validators),
            Some(residents),
        )
    }
    fn ctx_with_members(key: &[u8], members: Vec<Vec<u8>>) -> TestCtx {
        ctx_gated(key, members, Vec::new())
    }
    fn ctx_with_residents(key: &[u8], residents: Vec<Vec<u8>>) -> TestCtx {
        ctx_gated(key, Vec::new(), residents)
    }

    fn announce_with(tags: &[&str], resources: &[(&str, u64)]) -> Msg {
        Msg {
            target: "capability".into(),
            payload: encode_msg(&CapabilityMsg::Announce {
                capabilities: tags.iter().map(|t| t.to_string()).collect(),
                resources: resources.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            }),
        }
    }
    fn announce(tags: &[&str]) -> Msg {
        announce_with(tags, &[])
    }
    fn capable(c: &CapabilityRegistry, capability: &str, demands: &[(&str, u64)]) -> Vec<Vec<u8>> {
        let reply = futures::executor::block_on(c.query(&encode_query(
            &CapabilityQuery::CapableProviders {
                capability: capability.into(),
                demands: demands.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            },
        )))
        .unwrap();
        match crate::decode_reply(&reply).unwrap() {
            CapabilityReply::Providers(p) => p,
            other => panic!("expected Providers reply, got {other:?}"),
        }
    }
    fn node_tags(c: &CapabilityRegistry, node: &[u8]) -> Vec<String> {
        let reply = futures::executor::block_on(c.query(&encode_query(&CapabilityQuery::Node {
            node: node.to_vec(),
        })))
        .unwrap();
        match crate::decode_reply(&reply).unwrap() {
            CapabilityReply::Node(tags) => tags,
            other => panic!("expected Node reply, got {other:?}"),
        }
    }
    fn providers(c: &CapabilityRegistry, capability: &str) -> Vec<Vec<u8>> {
        let reply =
            futures::executor::block_on(c.query(&encode_query(&CapabilityQuery::Providers {
                capability: capability.into(),
            })))
            .unwrap();
        match crate::decode_reply(&reply).unwrap() {
            CapabilityReply::Providers(p) => p,
            other => panic!("expected Providers reply, got {other:?}"),
        }
    }
    /// the ctx-routed read lane real callers (saga's `assignment_pool`) go
    /// through — the standing re-check lives in [`CapabilityRegistry::
    /// query_with`], never in [`CapabilityRegistry::query`] alone.
    fn providers_with(c: &CapabilityRegistry, ctx: &TestCtx, capability: &str) -> Vec<Vec<u8>> {
        let reply = futures::executor::block_on(c.query_with(
            ctx,
            &encode_query(&CapabilityQuery::Providers {
                capability: capability.into(),
            }),
        ))
        .unwrap();
        match crate::decode_reply(&reply).unwrap() {
            CapabilityReply::Providers(p) => p,
            other => panic!("expected Providers reply, got {other:?}"),
        }
    }
    fn capable_providers_with(
        c: &CapabilityRegistry,
        ctx: &TestCtx,
        capability: &str,
        demands: &[(&str, u64)],
    ) -> Vec<Vec<u8>> {
        let reply = futures::executor::block_on(c.query_with(
            ctx,
            &encode_query(&CapabilityQuery::CapableProviders {
                capability: capability.into(),
                demands: demands.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            }),
        ))
        .unwrap();
        match crate::decode_reply(&reply).unwrap() {
            CapabilityReply::Providers(p) => p,
            other => panic!("expected Providers reply, got {other:?}"),
        }
    }
    /// an ungated registry (no valset) over a MemStore double — most tests
    /// exercise state mechanics, not the member gate (the qmdb continuity
    /// proof lives in `tests/sync_round_trip.rs`).
    fn ungated() -> CapabilityRegistry {
        CapabilityRegistry::new("capability", Box::new(MemStore::new()), None)
    }

    /// the root of a store that never committed anything — the store-backed
    /// twin of the old ZERO sentinel.
    fn empty_root() -> StateRoot {
        ungated().root()
    }

    #[test]
    fn announce_registers_and_moves_root_off_empty() {
        let mut c = ungated();
        let me = vec![1u8; 32];
        let mut ctx = ctx_external(&me);
        let empty = empty_root();
        assert_eq!(c.root(), empty, "genesis registry is empty");

        futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex", "claude"]))).unwrap();
        // staged, not committed: root unmoved, but read-your-writes sees it.
        assert_eq!(c.root(), empty, "root reflects committed only");
        assert_eq!(node_tags(&c, &me), vec!["claude", "codex"]);

        futures::executor::block_on(c.commit_block()).unwrap();
        assert_ne!(c.root(), empty, "a committed announce moves root");
        assert_eq!(node_tags(&c, &me), vec!["claude", "codex"]);
        assert_eq!(providers(&c, "codex"), vec![me]);
    }

    #[test]
    fn announce_is_a_declarative_replace() {
        let mut c = ungated();
        let me = vec![2u8; 32];
        let mut ctx = ctx_external(&me);
        futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex", "claude"]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();

        futures::executor::block_on(c.execute(&mut ctx, &announce(&["gemini"]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();

        assert_eq!(node_tags(&c, &me), vec!["gemini"], "full replace");
        assert!(providers(&c, "codex").is_empty(), "old tags dropped");
    }

    #[test]
    fn empty_announce_removes_the_node() {
        let mut c = ungated();
        let me = vec![3u8; 32];
        let mut ctx = ctx_external(&me);
        let empty = empty_root();
        futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex"]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_ne!(c.root(), empty);

        futures::executor::block_on(c.execute(&mut ctx, &announce(&[]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert!(node_tags(&c, &me).is_empty(), "removed");
        assert_eq!(c.root(), empty, "an emptied registry is the empty root");

        // removing a node that never announced is a no-op that stages nothing.
        futures::executor::block_on(c.execute(&mut ctx, &announce(&[]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(c.root(), empty, "a no-op removal holds the root");
    }

    #[test]
    fn duplicate_tags_collapse() {
        let mut c = ungated();
        let me = vec![4u8; 32];
        let mut ctx = ctx_external(&me);
        futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex", "codex"]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(node_tags(&c, &me), vec!["codex"], "set semantics");
    }

    #[test]
    fn non_external_origins_are_rejected() {
        let mut c = ungated();
        let empty = empty_root();
        for origin in [sdk::Origin::Module("saga".into()), sdk::Origin::System] {
            let mut ctx = ctx_origin(origin);
            let err = futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex"])))
                .unwrap_err();
            assert!(
                matches!(err, Error::Module { ref reason, .. } if reason == "external_origin_required"),
                "got {err:?}"
            );
        }
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(c.root(), empty, "nothing was staged");
    }

    #[test]
    fn empty_external_keys_are_rejected() {
        let mut c = ungated();
        let empty = empty_root();
        let mut ctx = ctx_external(&[]);
        let err =
            futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex"]))).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "empty_origin_key"),
            "got {err:?}"
        );
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(c.root(), empty, "nothing was staged");
    }

    #[test]
    fn member_gate_rejects_non_members_and_admits_members() {
        let mut c = CapabilityRegistry::new(
            "capability",
            Box::new(MemStore::new()),
            Some("valset".into()),
        );
        let me = vec![5u8; 32];

        let mut outsider = ctx_with_members(&me, vec![vec![9u8; 32]]);
        let err = futures::executor::block_on(c.execute(&mut outsider, &announce(&["codex"])))
            .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "no_standing"),
            "got {err:?}"
        );

        let mut member = ctx_with_members(&me, vec![me.clone()]);
        futures::executor::block_on(c.execute(&mut member, &announce(&["codex"]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(providers(&c, "codex"), vec![me]);
    }

    #[test]
    fn member_gate_admits_residents_and_still_rejects_outsiders() {
        let mut c = CapabilityRegistry::new(
            "capability",
            Box::new(MemStore::new()),
            Some("valset".into()),
        );
        let resident = vec![6u8; 32];
        let outsider = vec![7u8; 32];

        // a resident (granted, not yet promoted) announces successfully.
        let mut ctx = ctx_with_residents(&resident, vec![resident.clone()]);
        futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex"]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(providers(&c, "codex"), vec![resident.clone()]);

        // an outsider (neither member nor resident) still rejects.
        let mut ctx = ctx_gated(&outsider, vec![], vec![resident.clone()]);
        let err =
            futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex"]))).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "no_standing"),
            "got {err:?}"
        );
    }

    #[test]
    fn an_empty_roster_answers_without_valset_but_cannot_announce() {
        let mut registry = CapabilityRegistry::new(
            "capability",
            Box::new(MemStore::new()),
            Some("valset".into()),
        );
        let mut ctx = ctx_external(&[7; 32]);
        assert!(providers_with(&registry, &ctx, "codex").is_empty());
        assert!(capable_providers_with(&registry, &ctx, "codex", &[]).is_empty());
        assert!(
            futures::executor::block_on(registry.execute(&mut ctx, &announce(&["codex"]))).is_err()
        );
    }

    /// A node that loses standing is excluded from provider queries even if
    /// its announcement remains stored. Nodes with standing still appear.
    #[test]
    fn providers_excludes_a_node_that_lost_standing_after_announcing() {
        let mut c = CapabilityRegistry::new(
            "capability",
            Box::new(MemStore::new()),
            Some("valset".into()),
        );
        let stripped = vec![10u8; 32];
        let standing = vec![11u8; 32];

        // both nodes announce while they hold standing.
        let mut ctx = ctx_with_members(&stripped, vec![stripped.clone(), standing.clone()]);
        futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex"]))).unwrap();
        let mut ctx = ctx_with_members(&standing, vec![stripped.clone(), standing.clone()]);
        futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex"]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();

        // the roster still holds both records — nothing deleted `stripped`.
        assert_eq!(
            providers(&c, "codex"),
            vec![stripped.clone(), standing.clone()]
        );

        // valset now reports only `standing` (revoke, leave, or a closed
        // resident window all collapse to the same "no longer in the set").
        let post_revoke = ctx_with_members(&stripped, vec![standing.clone()]);
        assert_eq!(
            providers_with(&c, &post_revoke, "codex"),
            vec![standing.clone()],
            "a stripped-standing node must not be returned"
        );
        assert_eq!(
            capable_providers_with(&c, &post_revoke, "codex", &[]),
            vec![standing],
            "CapableProviders re-checks standing too"
        );
    }

    #[test]
    fn malformed_tags_are_rejected() {
        let mut c = ungated();
        let me = vec![8u8; 32];
        let empty = empty_root();
        let too_long = "x".repeat(MAX_TAG_LEN + 1);
        for bad in ["", too_long.as_str(), "UPPER", "spa ce", "uni∂ode"] {
            let mut ctx = ctx_external(&me);
            let err =
                futures::executor::block_on(c.execute(&mut ctx, &announce(&[bad]))).unwrap_err();
            assert!(
                matches!(err, Error::Module { ref reason, .. } if reason == "bad_capability_tag"),
                "got {err:?} for {bad:?}"
            );
        }
        // too many tags rejects too.
        let many: Vec<String> = (0..=MAX_CAPABILITIES).map(|i| format!("t{i}")).collect();
        let many_refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let mut ctx = ctx_external(&me);
        let err =
            futures::executor::block_on(c.execute(&mut ctx, &announce(&many_refs))).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "capability_cap"),
            "got {err:?}"
        );
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(c.root(), empty, "rejected announcements staged nothing");
    }

    #[test]
    fn root_is_state_based_order_independent() {
        // two registries reach the same state via different op orders — the
        // MemStore root is a function of state alone, so they converge. (the
        // production qmdb root is op-log-derived; cross-validator equality
        // there comes from consensus ordering, pinned by the parity proof.)
        let (a, b) = (vec![21u8; 32], vec![22u8; 32]);

        let mut c1 = ungated();
        futures::executor::block_on(async {
            c1.execute(&mut ctx_external(&a), &announce(&["codex"]))
                .await
                .unwrap();
            c1.execute(&mut ctx_external(&b), &announce(&["claude"]))
                .await
                .unwrap();
            c1.commit_block().await.unwrap();
        });

        let mut c2 = ungated();
        futures::executor::block_on(async {
            c2.execute(&mut ctx_external(&b), &announce(&["claude"]))
                .await
                .unwrap();
            c2.execute(&mut ctx_external(&a), &announce(&["codex"]))
                .await
                .unwrap();
            c2.commit_block().await.unwrap();
        });

        assert_eq!(c1.root(), c2.root(), "root is f(state), order-independent");
    }

    #[test]
    fn atomicity_a_failed_block_rolls_back_the_stage() {
        let mut c = ungated();
        let me = vec![10u8; 32];
        let mut ctx = ctx_external(&me);
        let before = c.root();

        futures::executor::block_on(c.execute(&mut ctx, &announce(&["codex"]))).unwrap();
        // ... a later dispatch in the same block errors, so the host aborts:
        futures::executor::block_on(c.abort_block()).unwrap();

        assert!(
            node_tags(&c, &me).is_empty(),
            "aborted announce left nothing"
        );
        assert_eq!(c.root(), before, "root unchanged after a rolled-back block");
    }

    #[test]
    fn resources_are_stored_queryable_and_move_the_root() {
        let mut c = ungated();
        let me = vec![30u8; 32];
        let mut ctx = ctx_external(&me);
        futures::executor::block_on(c.execute(&mut ctx, &announce_with(&["codex"], &[]))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        let tags_only_root = c.root();

        futures::executor::block_on(c.execute(
            &mut ctx,
            &announce_with(&["codex"], &[("cores", 8), ("mem_gb", 32)]),
        ))
        .unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_ne!(
            c.root(),
            tags_only_root,
            "resources are part of the commitment"
        );

        let reply =
            futures::executor::block_on(c.query(&encode_query(&CapabilityQuery::Resources {
                node: me.clone(),
            })))
            .unwrap();
        match crate::decode_reply(&reply).unwrap() {
            CapabilityReply::Resources(r) => {
                assert_eq!(r.get("cores"), Some(&8));
                assert_eq!(r.get("mem_gb"), Some(&32));
            }
            other => panic!("expected Resources reply, got {other:?}"),
        }
    }

    // ---- capability classes -------------------------------------------------

    fn claim(class: &str) -> Msg {
        Msg {
            target: "capability".into(),
            payload: encode_msg(&CapabilityMsg::ClaimClass {
                class: class.into(),
            }),
        }
    }
    fn module_ctx(id: &str) -> TestCtx {
        ctx_origin(sdk::Origin::Module(id.into()))
    }
    fn class_owner_of(c: &CapabilityRegistry, class: &str) -> Option<ModuleId> {
        let reply =
            futures::executor::block_on(c.query(&encode_query(&CapabilityQuery::ResolveClass {
                class: class.into(),
            })))
            .unwrap();
        match crate::decode_reply(&reply).unwrap() {
            CapabilityReply::ClassOwner(owner) => owner,
            other => panic!("expected ClassOwner reply, got {other:?}"),
        }
    }
    fn classes(c: &CapabilityRegistry) -> Vec<(String, ModuleId)> {
        let reply =
            futures::executor::block_on(c.query(&encode_query(&CapabilityQuery::Classes))).unwrap();
        match crate::decode_reply(&reply).unwrap() {
            CapabilityReply::Classes(classes) => classes,
            other => panic!("expected Classes reply, got {other:?}"),
        }
    }

    #[test]
    fn claim_class_stages_then_commits_and_moves_root() {
        let mut c = ungated();
        let empty = empty_root();
        assert_eq!(class_owner_of(&c, "agent"), None);

        futures::executor::block_on(c.execute(&mut module_ctx("dispatch"), &claim("agent")))
            .unwrap();
        // staged, not committed: root unmoved, but read-your-writes sees it.
        assert_eq!(c.root(), empty, "root reflects committed only");
        assert_eq!(class_owner_of(&c, "agent"), Some("dispatch".into()));

        futures::executor::block_on(c.commit_block()).unwrap();
        assert_ne!(c.root(), empty, "a committed claim moves root");
        assert_eq!(class_owner_of(&c, "agent"), Some("dispatch".into()));

        // a second class from another module; Classes enumerates both, sorted.
        futures::executor::block_on(c.execute(&mut module_ctx("saga"), &claim("ai"))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(
            classes(&c),
            vec![
                ("agent".to_string(), "dispatch".to_string()),
                ("ai".to_string(), "saga".to_string()),
            ]
        );
    }

    #[test]
    fn first_claim_wins_reclaim_is_idempotent_rival_rejects() {
        let mut c = ungated();
        futures::executor::block_on(c.execute(&mut module_ctx("dispatch"), &claim("agent")))
            .unwrap();

        // a rival claim in the SAME block reads the stage (read-your-writes)
        // and rejects — first claim wins at stage time, not commit time.
        let err = futures::executor::block_on(c.execute(&mut module_ctx("saga"), &claim("agent")))
            .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "class_already_claimed"),
            "got {err:?}"
        );

        futures::executor::block_on(c.commit_block()).unwrap();
        let committed = c.root();

        // a rival claim on the COMMITTED class rejects identically...
        let err = futures::executor::block_on(c.execute(&mut module_ctx("saga"), &claim("agent")))
            .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "class_already_claimed"),
            "got {err:?}"
        );
        // ...while a re-claim by the OWNER is an idempotent no-op.
        futures::executor::block_on(c.execute(&mut module_ctx("dispatch"), &claim("agent")))
            .unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(c.root(), committed, "an idempotent re-claim holds the root");
        assert_eq!(class_owner_of(&c, "agent"), Some("dispatch".into()));
    }

    #[test]
    fn claim_class_rejects_external_and_system_origins() {
        let mut c = ungated();
        let empty = empty_root();
        for origin in [
            sdk::Origin::External(vec![1u8; 32]),
            sdk::Origin::External(Vec::new()),
            sdk::Origin::System,
        ] {
            let mut ctx = ctx_origin(origin);
            let err =
                futures::executor::block_on(c.execute(&mut ctx, &claim("agent"))).unwrap_err();
            assert!(
                matches!(err, Error::Module { ref reason, .. } if reason == "module_origin_required"),
                "got {err:?}"
            );
        }
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(c.root(), empty, "nothing was staged");
    }

    #[test]
    fn malformed_classes_are_rejected() {
        let mut c = ungated();
        let empty = empty_root();
        let too_long = "c".repeat(MAX_CLASS_LEN + 1);
        for bad in [
            "",
            too_long.as_str(),
            "Agent",
            "ag:ent",
            "ag/ent",
            "ag.ent",
            "ag_ent",
        ] {
            let err =
                futures::executor::block_on(c.execute(&mut module_ctx("dispatch"), &claim(bad)))
                    .unwrap_err();
            assert!(
                matches!(err, Error::Module { ref reason, .. } if reason == "bad_class"),
                "got {err:?} for {bad:?}"
            );
        }
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(c.root(), empty, "rejected claims staged nothing");
    }

    #[test]
    fn abort_drops_staged_claims() {
        let mut c = ungated();
        let empty = empty_root();
        futures::executor::block_on(c.execute(&mut module_ctx("dispatch"), &claim("agent")))
            .unwrap();
        futures::executor::block_on(c.abort_block()).unwrap();
        assert_eq!(class_owner_of(&c, "agent"), None, "the stage is gone");
        assert_eq!(c.root(), empty, "root unchanged after abort");

        // the class is claimable again — the aborted claim never bound it.
        futures::executor::block_on(c.execute(&mut module_ctx("saga"), &claim("agent"))).unwrap();
        futures::executor::block_on(c.commit_block()).unwrap();
        assert_eq!(class_owner_of(&c, "agent"), Some("saga".into()));
    }

    #[test]
    fn capable_providers_filters_per_dimension_and_absent_is_not_infinite() {
        let mut c = ungated();
        let big = vec![31u8; 32];
        let small = vec![32u8; 32];
        let bare = vec![33u8; 32];
        futures::executor::block_on(async {
            c.execute(
                &mut ctx_external(&big),
                &announce_with(&["codex"], &[("cores", 16), ("mem_gb", 64)]),
            )
            .await
            .unwrap();
            c.execute(
                &mut ctx_external(&small),
                &announce_with(&["codex"], &[("cores", 4), ("mem_gb", 8)]),
            )
            .await
            .unwrap();
            // tags-only node (no announced capacity): never matches ANY demand.
            c.execute(&mut ctx_external(&bare), &announce_with(&["codex"], &[]))
                .await
                .unwrap();
            c.commit_block().await.unwrap();
        });

        assert_eq!(capable(&c, "codex", &[("cores", 8)]), vec![big.clone()]);
        // empty demands degrade to plain Providers (all three).
        assert_eq!(capable(&c, "codex", &[]).len(), 3);
        // a dimension nobody announced matches nobody.
        assert!(capable(&c, "codex", &[("gpu", 1)]).is_empty());
        // both dimensions must hold.
        assert_eq!(
            capable(&c, "codex", &[("cores", 4), ("mem_gb", 32)]),
            vec![big]
        );
    }

    #[test]
    fn resources_without_capabilities_reject_and_malformed_resources_reject() {
        let mut c = ungated();
        let me = vec![34u8; 32];
        let mut ctx = ctx_external(&me);
        // capacity with nothing to execute is meaningless — reject loudly.
        let err =
            futures::executor::block_on(c.execute(&mut ctx, &announce_with(&[], &[("cores", 8)])))
                .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "resources_without_tags"),
            "got {err:?}"
        );
        // zero value / bad key reject via validate_resources.
        assert!(
            futures::executor::block_on(
                c.execute(&mut ctx, &announce_with(&["codex"], &[("cores", 0)]))
            )
            .is_err()
        );
    }
}

// the wasm-guest port: the store-backed dispatch shell that adapts this
// module to the ducktape:module world. compiled only by the guest-builder's
// synthesized wasm32 cdylib workspace (feature `guest`), never by the native
// build.
#[cfg(feature = "guest")]
mod guest;
