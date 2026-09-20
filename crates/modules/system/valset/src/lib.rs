//! the ed25519 membership registry as replicated state: validators + residents.
//!
//! a validator is a 32-byte ed25519 public key. anyone holding a WELL-FORMED
//! ed25519 key may [`ValsetMsg::Join`] the set — no authorization, no gating,
//! no stake weighting. this is deliberately permissionless: per the design,
//! "permissionless joining suffices; don't concern with proper shares." real
//! governance (who may join) and stake-weighted shares (voting power) are
//! DEFERRED — this module only replicates *membership*.
//!
//! ## residents (staged admission)
//!
//! a RESIDENT holds mesh + statesync standing but no quorum seat — the tier
//! a joiner syncs in before promotion, so the consensus set only ever gains a
//! caught-up validator. [`ValsetMsg::Grant`] / [`ValsetMsg::Revoke`] manage
//! the set; a [`ValsetMsg::Join`] on a current resident PROMOTES it (adds the
//! validator, removes the resident, one block).
//!
//! ## state model
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: one tier record per
//! membership class — `validators` and `residents`, each a borsh-encoded
//! strictly-sorted key list, and an EMPTY tier is an ABSENT record, so a
//! given membership has exactly one record-set encoding. writes are staged
//! during a block (read-your-writes via [`sdk::StagedStore`]) and flushed in
//! one batch at `commit_block`; `abort_block` drops the stage; the module
//! root IS the store's committed merkle root, and sync belongs to the store.
//!
//! ## the mesh-generation window
//!
//! every committed block that changed membership advances a GENERATION
//! counter and snapshots the block's final `(validators, residents)` pair;
//! the last [`RETAINED_GENERATIONS`] snapshots are retained and served by
//! [`ValsetQuery::MeshWindow`]. the mesh transport tracks peer sets at
//! generation indices, so every node — however it arrived at the tip —
//! derives the IDENTICAL tracked window from this replicated state. one
//! generation per BLOCK, not per op: intermediate per-op snapshots would be
//! intra-block-order-dependent and could fork the root. generation 0 is the
//! genesis membership: the descriptor's fingerprinted validator list,
//! derivable by a joiner before it has synced anything.
//!
//! the observation barrier and the epoch cutover key on this module's root
//! MOVING, so an idempotent no-op — a re-join, a leave of an absent key, a
//! re-grant, a revoke of a non-resident — must STAGE NOTHING: a
//! byte-identical overwrite is still a committed qmdb op and would move the
//! root, splitting a drain batch (and waking the orchestrator) over nothing.
//!
//! ## state-sync
//!
//! a joiner rebuilds this module through the store's qmdb resolver lane
//! ([`sdk::StateSyncHandle::ResolverBacked`]): proof-carrying op ranges,
//! merkle-verified against the root consensus agreed on — the root, not the
//! serving peer, stays the trust anchor.

// the wire surface: this module's types, flattened at the crate root.
#[cfg(all(feature = "guest", target_arch = "wasm32"))]
mod guest;

mod wire;
pub use wire::*;

use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::PublicKey;
use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, ResolverSyncTarget, StagedStore, StateRoot,
    StateSyncHandle,
};

/// a 32-byte ed25519 public key encoding.
const KEY_LEN: usize = 32;

/// serialized tier-record byte bound — the uniform poison backstop on top of
/// the count cap.
const MAX_TIER_RECORD_BYTES: usize = 512 * 1024;

/// the committed validator tier's record key: the strictly-sorted 32-byte
/// member keys, borsh-encoded.
const VALIDATORS_KEY: &[u8] = b"validators";
/// the committed resident tier's record key, same shape.
const RESIDENTS_KEY: &[u8] = b"residents";

/// the membership generation counter's record key: a borsh `u64`, advanced by
/// exactly one for each committed block that changed a tier. ABSENT reads as
/// 0 — only reachable on a never-seeded store.
const GENERATION_KEY: &[u8] = b"generation";

/// generations whose snapshots are retained (and served by
/// [`ValsetQuery::MeshWindow`]). pinned to the mesh transport's
/// `tracked_peer_sets` depth by a node-side test — the two moving apart
/// would let one side track sets the other has already pruned.
pub const RETAINED_GENERATIONS: u64 = 4;

/// one generation snapshot's record key: `generation/` ++ big-endian `g`.
/// big-endian so the store's key order matches numeric order.
fn generation_set_key(generation: u64) -> Vec<u8> {
    let mut key = b"generation/".to_vec();
    key.extend_from_slice(&generation.to_be_bytes());
    key
}

/// generation 0 is the GENESIS membership: the descriptor's fingerprinted
/// validator list, and nothing else. this equality is the correctness anchor
/// for a joiner's pre-sync index-0 track — a node that has synced nothing yet
/// derives the same snapshot from its descriptor that every member carries in
/// state.
const GENESIS_GENERATION: u64 = 0;

pub struct Valset {
    id: ModuleId,
    /// the ONE module id whose follow-ups may stage a membership change.
    /// every module origin is a host-stamped follow-up from whichever guest
    /// just executed, so accepting `Module(_)` would let ANY admitted module
    /// add itself to the validator set. genesis wiring — identical on every
    /// node.
    governance_id: ModuleId,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`). store key
    /// is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl Valset {
    /// wrap the host-constructed store under module identity `id`.
    pub fn new(
        id: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
        governance_id: impl Into<ModuleId>,
    ) -> Self {
        Self {
            id: id.into(),
            governance_id: governance_id.into(),
            staged: StagedStore::new(store),
        }
    }

    /// GENESIS seeding: stage one founding validator BEFORE the host registers
    /// this instance; [`Valset::finish_seed`] publishes the whole seed set in
    /// one batch. deterministic and identical on every node (a different seed
    /// set composes a different genesis root-hash and the network forks at
    /// genesis). never valid after genesis: live changes go through the
    /// governance-gated `Join`/`Leave`/`Grant`/`Revoke` ops.
    ///
    /// does NOT curve-validate — genesis callers are trusted (production
    /// seeds from typed `ed25519::PublicKey`s); the `execute(Join)` path
    /// validates. the length assert stays: a wrong-width key is a wiring
    /// bug, never data.
    pub async fn seed(&mut self, key: Vec<u8>) -> Result<(), Error> {
        assert_eq!(
            key.len(),
            KEY_LEN,
            "genesis validator key must be {KEY_LEN} bytes"
        );
        let mut validators = self.validators().await?;
        let Err(position) = validators.binary_search(&key) else {
            return Ok(()); // re-seeding a key already in the set is a no-op.
        };
        Self::require_capacity(&validators, "validator")?;
        validators.insert(position, key);
        self.store_tier(VALIDATORS_KEY, &validators)
    }

    /// publish the staged genesis seed in one batch — idempotent: a store
    /// that already carries a generation counter (a reopened workspace
    /// re-entering the genesis path) is left byte-untouched, exactly like
    /// the modules registry's `finish_seed`. the gate keys on [`GENERATION_KEY`] rather
    /// than the validator tier because an EMPTY genesis set still commits
    /// the counter (and no snapshot) — the counter record is the one write
    /// every genesis performs.
    pub async fn finish_seed(&mut self) -> Result<(), Error> {
        let already_seeded = self.staged.get_committed(GENERATION_KEY).await?.is_some();
        if already_seeded {
            self.staged.abort();
            return Ok(());
        }
        let seeded_validators = self.validators().await?;
        self.stage_generation_counter(GENESIS_GENERATION);
        if !seeded_validators.is_empty() {
            self.stage_generation_snapshot(GENESIS_GENERATION, &seeded_validators, &Vec::new())?;
        }
        self.staged.commit().await
    }

    /// validate that `key` is a well-formed 32-byte ed25519 public key. the
    /// explicit length guard makes the 32-byte invariant independent of decode's
    /// trailing-byte behavior; `PublicKey::decode` then checks the curve point
    /// (ZIP215: must decompress to a point on the twisted Edwards curve).
    fn validate_key(key: &[u8]) -> Result<(), Error> {
        if key.len() != KEY_LEN {
            return Err(Error::module(
                "bad_key",
                format!(
                    "invalid ed25519 public key: expected {KEY_LEN} bytes, got {}",
                    key.len()
                ),
            ));
        }
        PublicKey::decode(key)
            .map_err(|e| Error::module("bad_key", format!("invalid ed25519 public key: {e}")))?;
        Ok(())
    }

    /// the count cap shared by both tiers.
    fn require_capacity(tier: &[Vec<u8>], what: &str) -> Result<(), Error> {
        if tier.len() >= MAX_MEMBERS {
            return Err(Error::module(
                "member_cap",
                format!("{what} cap reached ({MAX_MEMBERS})"),
            ));
        }
        Ok(())
    }

    // ---- staged-over-committed tier records ---------------------------------

    /// one tier's staged-over-committed view: the strictly-sorted member list,
    /// empty when the record is absent. a later op in the same block sees an
    /// earlier op's staged write (read-your-writes).
    async fn tier(&self, key: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let Some(bytes) = self.staged.get(key).await? else {
            return Ok(Vec::new());
        };
        borsh::from_slice(&bytes).map_err(|e| Error::module("codec", e.to_string()))
    }

    async fn validators(&self) -> Result<Vec<Vec<u8>>, Error> {
        self.tier(VALIDATORS_KEY).await
    }

    async fn residents(&self) -> Result<Vec<Vec<u8>>, Error> {
        self.tier(RESIDENTS_KEY).await
    }

    /// stage one tier record. an EMPTY tier stages a DELETE — absence is the
    /// single canonical encoding of "no members", so a fresh store and an
    /// emptied tier answer reads identically.
    fn store_tier(&mut self, key: &[u8], tier: &Vec<Vec<u8>>) -> Result<(), Error> {
        if tier.is_empty() {
            self.staged.delete(key.to_vec());
            return Ok(());
        }
        let bytes = borsh::to_vec(tier).expect("a member list is serializable");
        if bytes.len() > MAX_TIER_RECORD_BYTES {
            return Err(Error::module(
                "record_too_large",
                format!(
                    "tier record too large: {} > {MAX_TIER_RECORD_BYTES} bytes",
                    bytes.len()
                ),
            ));
        }
        self.staged.stage(key.to_vec(), bytes);
        Ok(())
    }

    // ---- the membership generation window ------------------------------------

    /// the staged-over-committed generation counter. ABSENT reads as 0 (a
    /// never-seeded store).
    async fn generation(&self) -> Result<u64, Error> {
        let Some(bytes) = self.staged.get(GENERATION_KEY).await? else {
            return Ok(GENESIS_GENERATION);
        };
        borsh::from_slice(&bytes).map_err(|e| Error::module("codec", e.to_string()))
    }

    fn stage_generation_counter(&mut self, generation: u64) {
        let bytes = borsh::to_vec(&generation).expect("a u64 is serializable");
        self.staged.stage(GENERATION_KEY.to_vec(), bytes);
    }

    /// stage one generation's membership snapshot. the pair rides one record
    /// so a generation is atomic — there is no state where its validators
    /// and residents disagree about which transition they describe.
    fn stage_generation_snapshot(
        &mut self,
        generation: u64,
        validators: &Vec<Vec<u8>>,
        residents: &Vec<Vec<u8>>,
    ) -> Result<(), Error> {
        let bytes =
            borsh::to_vec(&(validators, residents)).expect("a member snapshot is serializable");
        if bytes.len() > MAX_TIER_RECORD_BYTES {
            return Err(Error::module(
                "record_too_large",
                format!(
                    "generation snapshot too large: {} > {MAX_TIER_RECORD_BYTES} bytes",
                    bytes.len()
                ),
            ));
        }
        self.staged.stage(generation_set_key(generation), bytes);
        Ok(())
    }

    /// whether this block's staged tiers differ from the committed tiers — the
    /// commit-time predicate deciding a generation advance. a pure
    /// read-compare, deliberately NOT a dirty marker set by the handlers:
    /// a net-zero block (a leave and a re-join of the same key) stages tier
    /// writes yet changes no membership, and must not burn a generation.
    async fn staged_membership_changed(&self) -> Result<bool, Error> {
        let validators_changed = self.staged.get(VALIDATORS_KEY).await?
            != self.staged.get_committed(VALIDATORS_KEY).await?;
        if validators_changed {
            return Ok(true);
        }
        let residents_changed = self.staged.get(RESIDENTS_KEY).await?
            != self.staged.get_committed(RESIDENTS_KEY).await?;
        Ok(residents_changed)
    }

    /// advance the membership generation by one and snapshot the block's
    /// final tiers. called from `commit_block`, AT MOST ONCE PER BLOCK, and
    /// only when the block changed membership — never on a no-op block,
    /// preserving the module's stage-nothing invariant (the doc at the top
    /// of this file). one generation per block (not per op) keeps intra-block
    /// op order unable to fork the root: intermediate per-op snapshots would
    /// be order-dependent, the block's final membership is not. prunes the
    /// snapshot falling out of the retained window; the prune is
    /// existence-checked so it never stages a delete of an absent record.
    async fn advance_generation(&mut self) -> Result<(), Error> {
        let current = self.generation().await?;
        let next = current
            .checked_add(1)
            .expect("the membership generation counter overflowed u64");
        self.stage_generation_counter(next);
        let validators = self.validators().await?;
        let residents = self.residents().await?;
        self.stage_generation_snapshot(next, &validators, &residents)?;
        let Some(stale) = next.checked_sub(RETAINED_GENERATIONS) else {
            return Ok(());
        };
        let stale_key = generation_set_key(stale);
        if self.staged.get(&stale_key).await?.is_some() {
            self.staged.delete(stale_key);
        }
        Ok(())
    }

    /// the retained window, ascending: every present snapshot in
    /// `[latest - (RETAINED_GENERATIONS-1), latest]`. absent entries — pruned,
    /// or the empty-genesis case where no generation-0 snapshot exists — are
    /// skipped, never invented.
    async fn mesh_window(&self) -> Result<Vec<GenerationSet>, Error> {
        let latest = self.generation().await?;
        let from = latest.saturating_sub(RETAINED_GENERATIONS - 1);
        let mut window = Vec::new();
        for generation in from..=latest {
            let Some(bytes) = self.staged.get(&generation_set_key(generation)).await? else {
                continue;
            };
            let (validators, residents): (Vec<Vec<u8>>, Vec<Vec<u8>>) =
                borsh::from_slice(&bytes).map_err(|e| Error::module("codec", e.to_string()))?;
            window.push(GenerationSet {
                generation,
                validators,
                residents,
            });
        }
        Ok(window)
    }

    // ---- membership op handlers ---------------------------------------------

    async fn handle_join(&mut self, key: Vec<u8>) -> Result<(), Error> {
        Self::validate_key(&key)?;
        // PROMOTION: a joining resident leaves the resident tier in the same
        // block — one boundary carries the whole transition, and the
        // transport union never double-counts the key.
        let mut residents = self.residents().await?;
        if let Ok(position) = residents.binary_search(&key) {
            residents.remove(position);
            self.store_tier(RESIDENTS_KEY, &residents)?;
        }
        let mut validators = self.validators().await?;
        let Err(position) = validators.binary_search(&key) else {
            return Ok(()); // an idempotent re-join stages nothing.
        };
        Self::require_capacity(&validators, "validator")?;
        validators.insert(position, key);
        self.store_tier(VALIDATORS_KEY, &validators)
    }

    async fn handle_leave(&mut self, key: Vec<u8>) -> Result<(), Error> {
        let mut validators = self.validators().await?;
        let Ok(position) = validators.binary_search(&key) else {
            return Ok(()); // removing an absent key is a documented no-op.
        };
        // the validator set must NEVER go empty. a downstream orderer
        // reconfigured to zero validators hits commonware `quorum(0)`, which
        // panics ("n must not be zero") and halts the node. refuse a removal
        // that would drop the LAST validator. authoritative here: every
        // membership removal (a governance-passed RemoveValidator or genesis
        // orchestration) funnels through this arm, so the invariant holds no
        // matter who staged it — the set is closed under this rule regardless
        // of the caller. the guard reads the staged-over-committed tier, so a
        // second leave in the same block cannot slip past it.
        if validators.len() == 1 {
            return Err(Error::module(
                "last_validator",
                "refusing to remove the last validator: the set must never be empty",
            ));
        }
        validators.remove(position);
        self.store_tier(VALIDATORS_KEY, &validators)
    }

    async fn handle_grant(&mut self, key: Vec<u8>) -> Result<(), Error> {
        Self::validate_key(&key)?;
        // a validator already holds every resident capability; a second
        // standing would only smear the promote/demote edges.
        let validators = self.validators().await?;
        if validators.binary_search(&key).is_ok() {
            return Err(Error::module(
                "already_a_validator",
                "key is a current validator — resident standing is the pre-promotion tier",
            ));
        }
        let mut residents = self.residents().await?;
        let Err(position) = residents.binary_search(&key) else {
            return Ok(()); // an idempotent re-grant stages nothing.
        };
        Self::require_capacity(&residents, "resident")?;
        residents.insert(position, key);
        self.store_tier(RESIDENTS_KEY, &residents)
    }

    async fn handle_revoke(&mut self, key: Vec<u8>) -> Result<(), Error> {
        let mut residents = self.residents().await?;
        let Ok(position) = residents.binary_search(&key) else {
            return Ok(()); // revoking a non-resident is a documented no-op.
        };
        residents.remove(position);
        self.store_tier(RESIDENTS_KEY, &residents)
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Valset {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the store's committed merkle root over both tier records, verbatim —
    /// the staged overlay is invisible here until `commit_block`. the
    /// observation barrier compares this per drained block, so it moves
    /// exactly when committed membership changes.
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

    async fn initialize(&mut self, params: &[u8]) -> Result<(), Error> {
        let config = sdk::genesis_config::decode_config(params)?;
        let validators: Vec<Vec<u8>> = match sdk::genesis_config::find(&config, "validators") {
            Some(bytes) => sdk::wire::decode(bytes).map_err(|e| Error::module("codec", e))?,
            None => Vec::new(),
        };
        for key in validators {
            if key.len() != KEY_LEN {
                return Err(Error::module(
                    "bad_key",
                    "initial validator key must be 32 bytes",
                ));
            }
            self.seed(key).await?;
        }
        self.finish_seed().await
    }

    async fn flush_operation(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        // membership changes are GOVERNANCE-GATED: only the GOVERNANCE module's
        // own follow-up (after a passing proposal) or a system origin (genesis
        // orchestration) may stage them. an unauthenticated external Leave was
        // a one-message liveness kill on a private network; a bare `Module(_)`
        // was the same kill from any admitted module, whose follow-ups the host
        // stamps with its own id. origin is part of the deterministic Env, so
        // every validator enforces this identically.
        match &ctx.env().origin {
            sdk::Origin::Module(id) if *id == self.governance_id => {}
            sdk::Origin::System => {}
            other => {
                return Err(Error::module(
                    "not_governance",
                    format!(
                        "valset membership changes only via governance (the {} module), \
                         got {other:?}",
                        self.governance_id
                    ),
                ));
            }
        }
        match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
            ValsetMsg::Join { key } => self.handle_join(key).await,
            ValsetMsg::Leave { key } => self.handle_leave(key).await,
            ValsetMsg::Grant { key } => self.handle_grant(key).await,
            ValsetMsg::Revoke { key } => self.handle_revoke(key).await,
        }
    }

    /// read projection — the committed tiers plus this block's staged changes.
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(|e| Error::module("codec", e))? {
            ValsetQuery::Validators => Ok(encode_reply(&ValsetReply::Validators(
                self.validators().await?,
            ))),
            ValsetQuery::Residents => Ok(encode_reply(&ValsetReply::Residents(
                self.residents().await?,
            ))),
            ValsetQuery::MeshWindow => Ok(encode_reply(&ValsetReply::MeshWindow(
                self.mesh_window().await?,
            ))),
        }
    }

    /// publish the block's staged membership changes in ONE store batch —
    /// `root()` now reflects them. no-op (and no root movement) if nothing
    /// was staged. a block that changed membership advances the generation
    /// window in the same batch, so a committed root always carries a
    /// snapshot of the membership it describes.
    async fn commit_block(&mut self) -> Result<(), Error> {
        if self.staged_membership_changed().await? {
            self.advance_generation().await?;
        }
        self.staged.commit().await
    }

    /// discard the block's staged changes — committed state (and `root()`) is
    /// unchanged, so a failed block leaves no trace.
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{encode_msg, encode_query};
    use commonware_cryptography::Signer as _;
    use commonware_cryptography::ed25519::PrivateKey;

    use sdk_testkit::TestCtx;

    // valset's execute reads only env (origin); me/height are cosmetic, so
    // the shared TestCtx's defaults stand in.
    fn sys_ctx() -> TestCtx {
        TestCtx::at_height(0)
    }

    // a deterministic, VALID 32-byte ed25519 public key: any 32 bytes is a valid
    // ed25519 seed, and the derived public key is always a valid curve point.
    // u16 seeds so the cap test can mint more than 256 distinct keys.
    fn valid_key(seed: u16) -> Vec<u8> {
        let mut bytes = [0u8; 32];
        bytes[..2].copy_from_slice(&seed.to_le_bytes());
        let sk = PrivateKey::decode(&bytes[..]).expect("any 32 bytes is a valid seed");
        sk.public_key().as_ref().to_vec()
    }

    /// a ctx whose origin is another module's host-stamped follow-up.
    fn module_ctx(module_id: &str) -> TestCtx {
        TestCtx::with_env(sdk::Env {
            height: 0,
            consensus_time: 0,
            origin: sdk::Origin::Module(module_id.into()),
            me: "valset".into(),
            cause: sdk::Cause::Direct,
        })
    }

    fn fresh() -> Valset {
        Valset::new(
            "valset",
            Box::new(sdk_testkit::MemStore::new()),
            "governance",
        )
    }

    /// the root of a store that never committed anything — the store-backed
    /// twin of the old ZERO sentinel.
    fn empty_root() -> StateRoot {
        fresh().root()
    }

    fn join(key: &[u8]) -> Msg {
        Msg {
            target: "valset".into(),
            payload: encode_msg(&ValsetMsg::Join { key: key.to_vec() }),
        }
    }
    fn leave(key: &[u8]) -> Msg {
        Msg {
            target: "valset".into(),
            payload: encode_msg(&ValsetMsg::Leave { key: key.to_vec() }),
        }
    }
    fn grant(key: &[u8]) -> Msg {
        Msg {
            target: "valset".into(),
            payload: encode_msg(&ValsetMsg::Grant { key: key.to_vec() }),
        }
    }
    fn revoke(key: &[u8]) -> Msg {
        Msg {
            target: "valset".into(),
            payload: encode_msg(&ValsetMsg::Revoke { key: key.to_vec() }),
        }
    }
    fn run(v: &mut Valset, ctx: &mut TestCtx, m: &Msg) -> Result<(), Error> {
        futures::executor::block_on(v.execute(ctx, m))
    }
    fn commit(v: &mut Valset) {
        futures::executor::block_on(v.commit_block()).unwrap();
    }
    fn validators(v: &Valset) -> Vec<Vec<u8>> {
        let reply =
            futures::executor::block_on(v.query(&encode_query(&ValsetQuery::Validators))).unwrap();
        match crate::decode_reply(&reply).unwrap() {
            ValsetReply::Validators(list) => list,
            other => panic!("expected Validators, got {other:?}"),
        }
    }
    fn residents(v: &Valset) -> Vec<Vec<u8>> {
        let reply =
            futures::executor::block_on(v.query(&encode_query(&ValsetQuery::Residents))).unwrap();
        match crate::decode_reply(&reply).unwrap() {
            ValsetReply::Residents(list) => list,
            other => panic!("expected Residents, got {other:?}"),
        }
    }
    fn mesh_window(v: &Valset) -> Vec<GenerationSet> {
        let reply =
            futures::executor::block_on(v.query(&encode_query(&ValsetQuery::MeshWindow))).unwrap();
        match crate::decode_reply(&reply).unwrap() {
            ValsetReply::MeshWindow(window) => window,
            other => panic!("expected MeshWindow, got {other:?}"),
        }
    }

    /// membership is GOVERNANCE-gated, not module-gated: an admitted app
    /// module's follow-up carries its own id and must not seat itself.
    #[test]
    fn a_non_governance_module_changes_no_membership() {
        let mut v = fresh();
        let k = valid_key(1);
        let mut chat = module_ctx("chat");
        for m in [join(&k), leave(&k), grant(&k), revoke(&k)] {
            assert!(
                matches!(run(&mut v, &mut chat, &m), Err(Error::Module { ref reason, .. }) if reason == "not_governance"),
                "a chat-module origin staged a membership change"
            );
        }
        assert_eq!(v.root(), empty_root(), "nothing staged, nothing committed");

        let mut gov = module_ctx("governance");
        run(&mut v, &mut gov, &join(&k)).unwrap();
        commit(&mut v);
        assert_eq!(validators(&v), vec![k]);
    }

    #[test]
    fn join_adds_a_validator_and_moves_root_off_empty() {
        let mut v = fresh();
        let mut ctx = sys_ctx();
        assert_eq!(v.root(), empty_root(), "genesis set is empty -> empty root");

        let k = valid_key(1);
        run(&mut v, &mut ctx, &join(&k)).unwrap();
        // staged, not yet committed: root unchanged, but read-your-writes sees it.
        assert_eq!(v.root(), empty_root(), "root reflects committed only");
        assert_eq!(
            validators(&v),
            vec![k.clone()],
            "read-your-writes sees the stage"
        );

        commit(&mut v);
        assert_ne!(
            v.root(),
            empty_root(),
            "a committed join moves the root off the empty root"
        );
        assert_eq!(validators(&v), vec![k]);
    }

    #[test]
    fn leave_removes_a_validator() {
        // remove ONE of two validators — the set stays non-empty, so the
        // last-validator guard never fires. (removing the last validator is a
        // refused no-op; see `leaving_the_last_validator_is_refused`.)
        let mut v = fresh();
        let mut ctx = sys_ctx();
        let (keep, drop) = (valid_key(2), valid_key(3));
        run(&mut v, &mut ctx, &join(&keep)).unwrap();
        run(&mut v, &mut ctx, &join(&drop)).unwrap();
        commit(&mut v);
        let joined_root = v.root();

        run(&mut v, &mut ctx, &leave(&drop)).unwrap();
        commit(&mut v);
        assert_eq!(validators(&v), vec![keep], "leave removes exactly that key");
        assert_ne!(v.root(), joined_root, "the committed root moved");
        assert_ne!(
            v.root(),
            empty_root(),
            "a non-empty set is not the empty root"
        );
    }

    #[test]
    fn leaving_the_last_validator_is_refused() {
        // the set must never go empty: an orderer reconfigured to zero
        // validators hits commonware `quorum(0)`, which panics. removing the
        // SOLE validator is refused deterministically, and the set is untouched.
        let mut v = fresh();
        let mut ctx = sys_ctx();
        let solo = valid_key(7);
        run(&mut v, &mut ctx, &join(&solo)).unwrap();
        commit(&mut v);
        let before = v.root();

        let err = run(&mut v, &mut ctx, &leave(&solo)).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "last_validator"),
            "got {err:?}"
        );
        // read-your-writes: nothing was staged, so the sole validator remains.
        assert_eq!(validators(&v), vec![solo], "the last validator stays");
        commit(&mut v);
        assert_eq!(v.root(), before, "committed state is byte-identical");
    }

    #[test]
    fn leaving_the_last_of_a_shrinking_set_is_refused() {
        // stage two leaves in one block: the first (of two) is fine, the second
        // would empty the set within the same block's read-your-writes view and
        // is refused — the guard reads the STAGED-over-committed tier.
        let mut v = fresh();
        let mut ctx = sys_ctx();
        let (a, b) = (valid_key(4), valid_key(5));
        run(&mut v, &mut ctx, &join(&a)).unwrap();
        run(&mut v, &mut ctx, &join(&b)).unwrap();
        commit(&mut v);

        run(&mut v, &mut ctx, &leave(&a)).unwrap();
        let err = run(&mut v, &mut ctx, &leave(&b)).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "last_validator"),
            "got {err:?}"
        );
    }

    #[test]
    fn malformed_key_is_rejected() {
        let mut v = fresh();
        let mut ctx = sys_ctx();
        // wrong length is the deterministic malformed input: ~half of all 32-byte
        // strings are valid curve points (ZIP215 accepts non-canonical), so a
        // wrong-LENGTH key is the reliable reject path.
        let bad = vec![0u8; 16];
        let err = run(&mut v, &mut ctx, &join(&bad)).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "bad_key"),
            "malformed key errs with Module"
        );
        commit(&mut v);
        assert!(validators(&v).is_empty(), "a rejected join adds nothing");
        assert_eq!(v.root(), empty_root());
    }

    #[test]
    fn permissionless_any_valid_key_joins() {
        let mut v = fresh();
        let mut ctx = sys_ctx();
        // no authorization, no gating: three unrelated valid keys all join.
        for b in [10u16, 20, 30] {
            run(&mut v, &mut ctx, &join(&valid_key(b))).unwrap();
        }
        commit(&mut v);
        assert_eq!(
            validators(&v).len(),
            3,
            "any valid key joins, permissionlessly"
        );
    }

    #[test]
    fn intra_block_op_order_cannot_fork_the_root() {
        // two validators joined in opposite orders WITHIN one block: the
        // commit batch is key-sorted (BTreeMap iteration), so both instances
        // publish the identical record set and the identical root — stage
        // order inside a block can never fork per-node roots. (cross-block
        // op-ORDER equality on the real path-dependent qmdb root is the
        // replay-twin assertion in tests/sync_round_trip.rs.)
        let a = valid_key(3);
        let b = valid_key(4);

        let mut v1 = fresh();
        let mut c1 = sys_ctx();
        run(&mut v1, &mut c1, &join(&a)).unwrap();
        run(&mut v1, &mut c1, &join(&b)).unwrap();
        commit(&mut v1);

        let mut v2 = fresh();
        let mut c2 = sys_ctx();
        run(&mut v2, &mut c2, &join(&b)).unwrap();
        run(&mut v2, &mut c2, &join(&a)).unwrap();
        commit(&mut v2);

        assert_eq!(v1.root(), v2.root(), "same block, same root");
        assert_eq!(validators(&v1), validators(&v2), "same record set");
    }

    #[test]
    fn atomicity_a_failed_block_rolls_back_the_join() {
        // reuse the staging seam directly: stage a join, then the block
        // fails -> abort_block drops the stage. no validator is added, root is
        // byte-identical to its pre-block value.
        let mut v = fresh();
        let mut ctx = sys_ctx();
        let before = v.root();

        run(&mut v, &mut ctx, &join(&valid_key(5))).unwrap();
        // ... a later dispatch in the same block errors, so the host aborts:
        futures::executor::block_on(v.abort_block()).unwrap();

        assert!(validators(&v).is_empty(), "aborted join added no validator");
        assert_eq!(
            v.root(),
            before,
            "root is unchanged after a rolled-back block"
        );
    }

    #[test]
    fn idempotent_no_ops_stage_nothing_and_never_move_the_root() {
        // THE barrier-noise property: the observation barrier ends a drain
        // batch whenever this module's root moves, so a no-op re-join, an
        // absent-key leave, a re-grant, and a non-resident revoke must all
        // stage NOTHING — a byte-identical overwrite would still be a
        // committed qmdb op and would move the root.
        let mut v = fresh();
        let mut ctx = sys_ctx();
        let (member, resident) = (valid_key(1), valid_key(2));
        run(&mut v, &mut ctx, &join(&member)).unwrap();
        run(&mut v, &mut ctx, &grant(&resident)).unwrap();
        commit(&mut v);
        let settled = v.root();

        run(&mut v, &mut ctx, &join(&member)).unwrap();
        run(&mut v, &mut ctx, &leave(&valid_key(9))).unwrap();
        run(&mut v, &mut ctx, &grant(&resident)).unwrap();
        run(&mut v, &mut ctx, &revoke(&valid_key(9))).unwrap();
        commit(&mut v);

        assert_eq!(v.root(), settled, "no-ops committed nothing");
        assert_eq!(validators(&v), vec![member], "membership unchanged");
        assert_eq!(residents(&v), vec![resident], "residents unchanged");
        assert_eq!(
            mesh_window(&v).last().map(|s| s.generation),
            Some(1),
            "a no-op block burned no generation"
        );
    }

    #[test]
    fn net_zero_block_burns_no_generation() {
        // a REAL leave and a REAL re-join of the same key in one block: tier
        // writes are staged, but the block's final membership is identical to
        // the committed membership — the commit-time predicate compares
        // staged vs committed, so no generation advances.
        let mut v = fresh();
        let mut ctx = sys_ctx();
        let (a, b) = (valid_key(1), valid_key(2));
        run(&mut v, &mut ctx, &join(&a)).unwrap();
        run(&mut v, &mut ctx, &join(&b)).unwrap();
        commit(&mut v);
        assert_eq!(mesh_window(&v).last().unwrap().generation, 1);

        run(&mut v, &mut ctx, &leave(&b)).unwrap();
        run(&mut v, &mut ctx, &join(&b)).unwrap();
        commit(&mut v);
        assert_eq!(
            mesh_window(&v).last().unwrap().generation,
            1,
            "net-zero membership burned no generation"
        );
    }

    #[test]
    fn join_past_the_member_cap_is_refused() {
        let mut v = fresh();
        let mut ctx = sys_ctx();
        for seed in 0..MAX_MEMBERS as u16 {
            run(&mut v, &mut ctx, &join(&valid_key(seed))).unwrap();
        }
        let err = run(&mut v, &mut ctx, &join(&valid_key(MAX_MEMBERS as u16))).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "member_cap"),
            "got {err:?}"
        );
        commit(&mut v);
        assert_eq!(validators(&v).len(), MAX_MEMBERS, "the cap held");
    }

    // ---- genesis seeding ----------------------------------------------------

    #[test]
    fn genesis_seed_publishes_once_and_reseeding_is_a_no_op() {
        let mut v = fresh();
        let (a, b) = (valid_key(1), valid_key(2));
        futures::executor::block_on(async {
            v.seed(a.clone()).await.unwrap();
            v.seed(b.clone()).await.unwrap();
            v.finish_seed().await.unwrap();
        });
        let seeded = v.root();
        assert_ne!(seeded, empty_root(), "the seed set is committed state");
        assert_eq!(validators(&v).len(), 2);
        let window = mesh_window(&v);
        assert_eq!(
            window.iter().map(|s| s.generation).collect::<Vec<_>>(),
            vec![0],
            "genesis is generation 0"
        );
        assert_eq!(
            window[0].validators,
            validators(&v),
            "the generation-0 snapshot IS the seeded validator list"
        );
        assert!(window[0].residents.is_empty());

        // a reopened workspace re-entering the genesis path re-seeds — the
        // idempotence gate must leave the store byte-untouched.
        futures::executor::block_on(async {
            v.seed(valid_key(9)).await.unwrap();
            v.finish_seed().await.unwrap();
        });
        assert_eq!(
            v.root(),
            seeded,
            "re-seeding an initialized store is a no-op"
        );
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(
            validators(&v),
            expected,
            "the original seed survived the re-entry"
        );
    }

    #[test]
    #[should_panic(expected = "genesis validator key must be 32 bytes")]
    fn seeding_a_wrong_width_key_is_a_wiring_bug() {
        // the seed seam trusts its caller (production seeds typed ed25519
        // keys) but a wrong-WIDTH key can only be a wiring bug — loud panic,
        // never staged state.
        let mut v = fresh();
        let _ = futures::executor::block_on(v.seed(vec![0u8; 16]));
    }

    // ---- residents (staged admission) ----------------------------------------

    #[test]
    fn resident_ops_apply_from_genesis() {
        let mut v = fresh();
        let mut ctx = sys_ctx();
        run(&mut v, &mut ctx, &join(&valid_key(1))).unwrap();
        commit(&mut v);

        // grant confers resident standing on a freshly founded network.
        let obs = valid_key(2);
        run(&mut v, &mut ctx, &grant(&obs)).unwrap();
        commit(&mut v);
        assert_eq!(residents(&v), vec![obs.clone()], "grant applied");

        // revoke clears it.
        run(&mut v, &mut ctx, &revoke(&obs)).unwrap();
        commit(&mut v);
        assert!(residents(&v).is_empty(), "revoke applied");
    }

    #[test]
    fn grant_stages_then_commits_and_revoke_empties_the_tier() {
        // the staged/committed split on the resident tier: a grant is visible
        // to read-your-writes at once but moves the root only at commit;
        // revoking the last resident returns the VIEW to empty (the record is
        // deleted — absence is the one encoding of "no residents"). no
        // root-restoration assertion: the qmdb op-log root never returns to a
        // prior value after insert+delete.
        let mut v = fresh();
        let mut ctx = sys_ctx();
        run(&mut v, &mut ctx, &join(&valid_key(1))).unwrap();
        commit(&mut v);
        let validators_only = v.root();

        let obs = valid_key(2);
        run(&mut v, &mut ctx, &grant(&obs)).unwrap();
        assert_eq!(v.root(), validators_only, "root reflects committed only");
        assert_eq!(residents(&v), vec![obs.clone()], "read-your-writes");
        commit(&mut v);
        assert_ne!(
            v.root(),
            validators_only,
            "a committed grant moves the root"
        );

        run(&mut v, &mut ctx, &revoke(&obs)).unwrap();
        commit(&mut v);
        assert!(residents(&v).is_empty(), "the resident tier emptied");
        assert_eq!(validators(&v).len(), 1, "validators untouched");
    }

    #[test]
    fn join_promotes_a_resident_out_of_the_tier() {
        let mut v = fresh();
        let mut ctx = sys_ctx();
        run(&mut v, &mut ctx, &join(&valid_key(1))).unwrap();
        let obs = valid_key(2);
        run(&mut v, &mut ctx, &grant(&obs)).unwrap();
        commit(&mut v);
        assert_eq!(residents(&v), vec![obs.clone()]);

        // the promotion: ONE Join both seats the validator and clears the
        // resident standing, in the same block.
        run(&mut v, &mut ctx, &join(&obs)).unwrap();
        commit(&mut v);
        assert!(validators(&v).contains(&obs), "promoted into the quorum");
        assert!(residents(&v).is_empty(), "and out of the resident tier");
    }

    #[test]
    fn granting_a_current_validator_is_refused() {
        let mut v = fresh();
        let mut ctx = sys_ctx();
        let k = valid_key(1);
        run(&mut v, &mut ctx, &join(&k)).unwrap();
        commit(&mut v);

        let err = run(&mut v, &mut ctx, &grant(&k)).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "already_a_validator"),
            "got {err:?}"
        );
        assert!(residents(&v).is_empty());
    }

    // ---- the mesh-generation window ------------------------------------------

    fn sorted(mut keys: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        keys.sort();
        keys
    }

    #[test]
    fn each_changing_block_advances_one_generation_and_snapshots_it() {
        let mut v = fresh();
        let mut ctx = sys_ctx();
        let (a, r) = (valid_key(1), valid_key(2));

        run(&mut v, &mut ctx, &join(&a)).unwrap();
        commit(&mut v);
        run(&mut v, &mut ctx, &grant(&r)).unwrap();
        commit(&mut v);
        // PROMOTION is one block and therefore ONE generation, carrying both
        // tier edits.
        run(&mut v, &mut ctx, &join(&r)).unwrap();
        commit(&mut v);

        let window = mesh_window(&v);
        let generations: Vec<u64> = window.iter().map(|s| s.generation).collect();
        assert_eq!(
            generations,
            vec![1, 2, 3],
            "one generation per block, ascending"
        );
        assert_eq!(window[0].validators, vec![a.clone()]);
        assert!(window[0].residents.is_empty());
        assert_eq!(window[1].validators, vec![a.clone()]);
        assert_eq!(window[1].residents, vec![r.clone()]);
        assert_eq!(
            window[2].validators,
            sorted(vec![a, r]),
            "promotion landed in the validator tier"
        );
        assert!(
            window[2].residents.is_empty(),
            "promotion left the resident tier in the same generation"
        );
    }

    #[test]
    fn many_ops_in_one_block_burn_one_generation() {
        let mut v = fresh();
        let mut ctx = sys_ctx();
        let (a, b, r) = (valid_key(1), valid_key(2), valid_key(3));
        run(&mut v, &mut ctx, &join(&a)).unwrap();
        run(&mut v, &mut ctx, &join(&b)).unwrap();
        run(&mut v, &mut ctx, &grant(&r)).unwrap();
        commit(&mut v);

        let window = mesh_window(&v);
        assert_eq!(window.len(), 1, "one block, one generation");
        assert_eq!(window[0].generation, 1);
        assert_eq!(window[0].validators, sorted(vec![a, b]));
        assert_eq!(window[0].residents, vec![r]);
    }

    #[test]
    fn window_prunes_to_retained_depth() {
        // genesis seeds generation 0; five changing blocks advance to 5. the
        // window serves exactly the last RETAINED_GENERATIONS snapshots,
        // ascending — 0 and 1 are pruned records, not just filtered replies.
        let mut v = fresh();
        let mut ctx = sys_ctx();
        futures::executor::block_on(async {
            v.seed(valid_key(0)).await.unwrap();
            v.finish_seed().await.unwrap();
        });
        assert_eq!(
            mesh_window(&v)
                .iter()
                .map(|s| s.generation)
                .collect::<Vec<_>>(),
            vec![0],
            "genesis committed the generation-0 snapshot"
        );
        for seed in 1..=5u16 {
            run(&mut v, &mut ctx, &join(&valid_key(seed))).unwrap();
            commit(&mut v);
        }

        let window = mesh_window(&v);
        let generations: Vec<u64> = window.iter().map(|s| s.generation).collect();
        assert_eq!(
            generations,
            vec![2, 3, 4, 5],
            "retained depth is 4, ascending"
        );
        assert_eq!(
            window.last().unwrap().validators.len(),
            6,
            "the tip snapshot carries the full membership"
        );
    }

    #[test]
    fn empty_genesis_commits_the_counter_only() {
        // the production pin composes valset with an EMPTY seed set: genesis
        // still commits the generation counter (the idempotence gate keys on
        // it), but no generation-0 snapshot is invented for an empty set.
        let mut v = fresh();
        futures::executor::block_on(v.finish_seed()).unwrap();
        assert_ne!(v.root(), empty_root(), "the counter record is committed");
        assert!(mesh_window(&v).is_empty(), "no snapshot for an empty set");

        // re-entering the genesis path is still byte-untouched.
        let sealed = v.root();
        futures::executor::block_on(v.finish_seed()).unwrap();
        assert_eq!(v.root(), sealed, "re-entry is a no-op");

        // the first real membership block advances to generation 1.
        let mut ctx = sys_ctx();
        run(&mut v, &mut ctx, &join(&valid_key(1))).unwrap();
        commit(&mut v);
        assert_eq!(
            mesh_window(&v)
                .iter()
                .map(|s| s.generation)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }
}
