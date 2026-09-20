//! the on-chain submit-policy federation: which [`Standing`] a target module
//! requires of an EXTERNAL submitter.
//!
//! ## the model
//!
//! the table is EMPTY at genesis and a missing entry (after the `"*"`
//! fallback) is OPEN — a fresh network admits any validly signed frame to any
//! module, exactly as it would with no acl module composed at all. the table
//! exists only to TIGHTEN, one governance proposal at a time, so who-may-do-
//! what changes carry a ballot trail instead of a release.
//!
//! ## enforcement — dispatch, not per-module
//!
//! this module only HOLDS policy. the gate lives where ops are routed to
//! modules: the kernel host's drain consults [`AclQuery::PolicyFor`] before
//! an `Origin::External` op reaches its target and resolves the origin's
//! principal against valset/identity. a failed check is a deterministic
//! rejection — the identical no-op every honest validator makes. modules keep
//! their own SEMANTIC origin checks (valset's governance-only gate, chat's
//! channel policies); this is the coarse standing gate above them, in one
//! place instead of N.
//!
//! ## state model
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: ONE `policy` record —
//! a borsh-encoded, strictly-target-sorted `Vec<(String, Standing)>` — and an
//! EMPTY table is an ABSENT record, so a given policy has exactly one
//! record-set encoding. writes are staged during a block (read-your-writes via
//! [`sdk::StagedStore`]) and flushed in one batch at `commit_block`; the
//! module root IS the store's committed merkle root, and sync belongs to the
//! store (the qmdb resolver lane, like every store-backed sibling).

// the wire surface: this module's shared types, flattened at the crate root.
mod wire;
pub use wire::*;

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;

use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore,
    StateRoot, StateSyncHandle,
};

/// policy entries retained (the count cap). targets are module ids, so this
/// sits far above any real composition; a set past it refuses loudly.
pub const MAX_POLICY_ENTRIES: usize = 256;

/// the committed policy table's record key: the strictly-target-sorted
/// `(target, standing)` list, borsh-encoded. absent = empty table = open.
const POLICY_KEY: &[u8] = b"policy";

pub struct Acl {
    id: ModuleId,
    /// the ONE module id whose follow-ups may stage a policy change. every
    /// module origin is a host-stamped follow-up from whichever guest just
    /// executed, so accepting `Module(_)` would let ANY admitted module
    /// rewrite the acl table for itself. genesis wiring — identical on every
    /// node.
    governance_id: ModuleId,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`). store key
    /// is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl Acl {
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

    /// the staged-over-committed policy table: strictly target-sorted, empty
    /// when the record is absent.
    async fn table(&self) -> Result<Vec<(String, Standing)>, Error> {
        let Some(bytes) = self.staged.get(POLICY_KEY).await? else {
            return Ok(Vec::new());
        };
        borsh::from_slice(&bytes).map_err(|e| Error::module("codec", e.to_string()))
    }

    /// stage the policy record. an EMPTY table stages a DELETE — absence is
    /// the single canonical encoding of "no policy", so a fresh store and a
    /// fully cleared table answer reads identically.
    fn store_table(&mut self, table: &Vec<(String, Standing)>) {
        if table.is_empty() {
            self.staged.delete(POLICY_KEY.to_vec());
            return;
        }
        // bounded by construction: ≤ MAX_POLICY_ENTRIES entries of ≤
        // MAX_TARGET_LEN-byte targets plus a one-byte standing tag.
        let bytes = borsh::to_vec(table).expect("a policy table is serializable");
        self.staged.stage(POLICY_KEY.to_vec(), bytes);
    }

    async fn handle_set_policy(
        &mut self,
        target: String,
        standing: Option<Standing>,
    ) -> Result<(), Error> {
        let trimmed_is_original = !target.is_empty() && target.trim() == target;
        if !trimmed_is_original {
            return Err(Error::module(
                "bad_acl_target",
                "acl target must be a non-empty, untrimmed module id",
            ));
        }
        if target.len() > MAX_TARGET_LEN {
            return Err(Error::module(
                "bad_acl_target",
                format!("acl target exceeds {MAX_TARGET_LEN} bytes"),
            ));
        }
        let mut table = self.table().await?;
        let position = table.binary_search_by(|(t, _)| t.as_str().cmp(target.as_str()));
        match (position, standing) {
            // an idempotent re-set stages nothing (no root movement).
            (Ok(i), Some(s)) if table[i].1 == s => return Ok(()),
            (Ok(i), Some(s)) => table[i].1 = s,
            (Ok(i), None) => {
                table.remove(i);
            }
            // clearing an absent entry is a documented no-op.
            (Err(_), None) => return Ok(()),
            (Err(i), Some(s)) => {
                if table.len() >= MAX_POLICY_ENTRIES {
                    return Err(Error::module(
                        "policy_cap",
                        format!("acl policy cap reached ({MAX_POLICY_ENTRIES})"),
                    ));
                }
                table.insert(i, (target, s));
            }
        }
        self.store_table(&table);
        Ok(())
    }

    /// the EFFECTIVE standing for `target`: the exact entry, else the `"*"`
    /// entry, else `None` (= open).
    async fn policy_for(&self, target: &str) -> Result<Option<Standing>, Error> {
        let table = self.table().await?;
        let lookup = |t: &str| {
            table
                .binary_search_by(|(entry, _)| entry.as_str().cmp(t))
                .ok()
                .map(|i| table[i].1)
        };
        Ok(lookup(target).or_else(|| lookup(WILDCARD_TARGET)))
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Acl {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the store's committed merkle root over the policy record, verbatim —
    /// the staged overlay is invisible here until `commit_block`.
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
        // policy changes are GOVERNANCE-GATED: only the GOVERNANCE module's
        // own follow-up (after a passing proposal) or a system origin (genesis
        // orchestration) may stage them. a bare `Module(_)` would let any
        // admitted module rewrite the acl table for itself — the host stamps
        // every module origin with the id of whichever guest just ran. origin
        // is part of the deterministic Env, so every validator enforces this
        // identically.
        match &ctx.env().origin {
            Origin::Module(id) if *id == self.governance_id => {}
            Origin::System => {}
            other => {
                return Err(Error::module(
                    "not_governance",
                    format!(
                        "acl policy changes only via governance (the {} module), got {other:?}",
                        self.governance_id
                    ),
                ));
            }
        }
        match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
            AclMsg::SetPolicy { target, standing } => {
                self.handle_set_policy(target, standing).await
            }
        }
    }

    /// read projection — the committed table plus this block's staged changes.
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(|e| Error::module("codec", e))? {
            AclQuery::Policy => Ok(encode_reply(&AclReply::Policy(self.table().await?))),
            AclQuery::PolicyFor { target } => Ok(encode_reply(&AclReply::PolicyFor(
                self.policy_for(&target).await?,
            ))),
        }
    }

    /// publish the block's staged policy changes in ONE store batch — `root()`
    /// now reflects them. no-op (and no root movement) if nothing was staged.
    async fn commit_block(&mut self) -> Result<(), Error> {
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
    use sdk_testkit::TestCtx;

    // acl's execute reads only env (origin); me/height are cosmetic, so the
    // shared TestCtx's defaults stand in for the module-origin path.
    fn gov_ctx() -> TestCtx {
        TestCtx::with_env(sdk::Env {
            height: 1,
            consensus_time: 1,
            origin: Origin::Module("governance".into()),
            me: DEFAULT_ACL_ID.into(),
            cause: sdk::Cause::Direct,
        })
    }

    fn ext_ctx() -> TestCtx {
        TestCtx::with_env(sdk::Env {
            height: 1,
            consensus_time: 1,
            origin: Origin::External(vec![7u8; 32]),
            me: DEFAULT_ACL_ID.into(),
            cause: sdk::Cause::Direct,
        })
    }

    fn fresh() -> Acl {
        Acl::new(
            DEFAULT_ACL_ID,
            Box::new(sdk_testkit::MemStore::new()),
            "governance",
        )
    }

    /// a ctx whose origin is another module's host-stamped follow-up.
    fn module_ctx(module_id: &str) -> TestCtx {
        TestCtx::with_env(sdk::Env {
            height: 1,
            consensus_time: 1,
            origin: Origin::Module(module_id.into()),
            me: DEFAULT_ACL_ID.into(),
            cause: sdk::Cause::Direct,
        })
    }

    /// the root of a store that never committed anything — the allow-all
    /// genesis shape.
    fn empty_root() -> StateRoot {
        fresh().root()
    }

    fn set(target: &str, standing: Option<Standing>) -> Msg {
        Msg {
            target: DEFAULT_ACL_ID.into(),
            payload: encode_msg(&AclMsg::SetPolicy {
                target: target.into(),
                standing,
            }),
        }
    }

    fn run(a: &mut Acl, ctx: &mut TestCtx, m: &Msg) -> Result<(), Error> {
        futures::executor::block_on(a.execute(ctx, m))
    }

    fn commit(a: &mut Acl) {
        futures::executor::block_on(a.commit_block()).unwrap();
    }

    fn policy_for(a: &Acl, target: &str) -> Option<Standing> {
        let reply = futures::executor::block_on(a.query(&encode_query(&AclQuery::PolicyFor {
            target: target.into(),
        })))
        .unwrap();
        match decode_reply(&reply).unwrap() {
            AclReply::PolicyFor(p) => p,
            other => panic!("expected PolicyFor, got {other:?}"),
        }
    }

    fn table(a: &Acl) -> Vec<(String, Standing)> {
        let reply = futures::executor::block_on(a.query(&encode_query(&AclQuery::Policy))).unwrap();
        match decode_reply(&reply).unwrap() {
            AclReply::Policy(t) => t,
            other => panic!("expected Policy, got {other:?}"),
        }
    }

    /// policy is GOVERNANCE-gated, not module-gated: an admitted app module's
    /// follow-up must not rewrite the acl table for itself. system origin
    /// (genesis orchestration) still passes.
    #[test]
    fn a_non_governance_module_sets_no_policy() {
        let mut a = fresh();
        let mut chat = module_ctx("chat");
        assert!(
            matches!(
                run(&mut a, &mut chat, &set("valset", Some(Standing::Validator))),
                Err(Error::Module { ref reason, .. }) if reason == "not_governance"
            ),
            "a chat-module origin set acl policy"
        );
        assert_eq!(a.root(), empty_root(), "nothing staged, nothing committed");

        let mut sys = TestCtx::with_env(sdk::Env {
            height: 1,
            consensus_time: 1,
            origin: Origin::System,
            me: DEFAULT_ACL_ID.into(),
            cause: sdk::Cause::Direct,
        });
        run(&mut a, &mut sys, &set("valset", Some(Standing::Validator))).unwrap();

        let mut gov = gov_ctx();
        run(&mut a, &mut gov, &set("chat", Some(Standing::User))).unwrap();
        commit(&mut a);
        assert_eq!(policy_for(&a, "valset"), Some(Standing::Validator));
        assert_eq!(policy_for(&a, "chat"), Some(Standing::User));
    }

    #[test]
    fn genesis_is_allow_all_and_a_set_entry_tightens_one_target() {
        let mut a = fresh();
        assert_eq!(a.root(), empty_root(), "genesis table is empty");
        assert_eq!(policy_for(&a, "chat"), None, "unlisted = open");

        let mut ctx = gov_ctx();
        run(&mut a, &mut ctx, &set("valset", Some(Standing::Validator))).unwrap();
        // staged, not yet committed: root unchanged, read-your-writes sees it.
        assert_eq!(a.root(), empty_root(), "root reflects committed only");
        assert_eq!(policy_for(&a, "valset"), Some(Standing::Validator));
        commit(&mut a);
        assert_ne!(a.root(), empty_root(), "a committed entry moves the root");
        assert_eq!(policy_for(&a, "valset"), Some(Standing::Validator));
        assert_eq!(policy_for(&a, "chat"), None, "other targets stay open");
    }

    #[test]
    fn the_wildcard_entry_is_the_fallback_and_an_exact_entry_beats_it() {
        let mut a = fresh();
        let mut ctx = gov_ctx();
        run(
            &mut a,
            &mut ctx,
            &set(WILDCARD_TARGET, Some(Standing::User)),
        )
        .unwrap();
        run(&mut a, &mut ctx, &set("chat", Some(Standing::Open))).unwrap();
        commit(&mut a);

        assert_eq!(policy_for(&a, "pages"), Some(Standing::User), "* fallback");
        assert_eq!(
            policy_for(&a, "chat"),
            Some(Standing::Open),
            "the exact entry wins over *"
        );
    }

    #[test]
    fn clearing_the_last_entry_restores_the_never_set_view() {
        let mut a = fresh();
        let mut ctx = gov_ctx();
        run(&mut a, &mut ctx, &set("forge", Some(Standing::Node))).unwrap();
        commit(&mut a);
        assert_eq!(table(&a).len(), 1);

        run(&mut a, &mut ctx, &set("forge", None)).unwrap();
        commit(&mut a);
        assert!(table(&a).is_empty(), "the table emptied");
        assert_eq!(policy_for(&a, "forge"), None, "back to open");
        // clearing an absent entry is a staged no-op (still Ok).
        let settled = a.root();
        run(&mut a, &mut ctx, &set("forge", None)).unwrap();
        commit(&mut a);
        assert_eq!(a.root(), settled, "a no-op clear commits nothing");
    }

    #[test]
    fn an_idempotent_re_set_stages_nothing() {
        let mut a = fresh();
        let mut ctx = gov_ctx();
        run(&mut a, &mut ctx, &set("chat", Some(Standing::User))).unwrap();
        commit(&mut a);
        let settled = a.root();

        run(&mut a, &mut ctx, &set("chat", Some(Standing::User))).unwrap();
        commit(&mut a);
        assert_eq!(a.root(), settled, "a duplicate set is a staged no-op");
    }

    #[test]
    fn an_external_origin_cannot_write_policy() {
        let mut a = fresh();
        let mut ctx = ext_ctx();
        let err = run(&mut a, &mut ctx, &set("chat", Some(Standing::Validator))).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "not_governance"),
            "got {err:?}"
        );
        assert!(table(&a).is_empty());
    }

    #[test]
    fn junk_targets_and_the_entry_cap_are_refused() {
        let mut a = fresh();
        let mut ctx = gov_ctx();
        for bad in ["", " chat", "chat "] {
            let err = run(&mut a, &mut ctx, &set(bad, Some(Standing::Open))).unwrap_err();
            assert!(
                matches!(err, Error::Module { ref reason, .. } if reason == "bad_acl_target"),
                "{bad:?} must refuse"
            );
        }
        let err = run(
            &mut a,
            &mut ctx,
            &set(&"x".repeat(MAX_TARGET_LEN + 1), Some(Standing::Open)),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "bad_acl_target"),
            "got {err:?}"
        );

        for n in 0..MAX_POLICY_ENTRIES {
            run(
                &mut a,
                &mut ctx,
                &set(&format!("m{n}"), Some(Standing::Open)),
            )
            .unwrap();
        }
        let err = run(&mut a, &mut ctx, &set("one-more", Some(Standing::Open))).unwrap_err();
        assert!(
            matches!(err, Error::Module { ref reason, .. } if reason == "policy_cap"),
            "got {err:?}"
        );
    }

    #[test]
    fn abort_block_drops_staged_policy() {
        let mut a = fresh();
        let mut ctx = gov_ctx();
        run(&mut a, &mut ctx, &set("chat", Some(Standing::Validator))).unwrap();
        assert_eq!(policy_for(&a, "chat"), Some(Standing::Validator), "staged");
        futures::executor::block_on(a.abort_block()).unwrap();
        assert_eq!(
            policy_for(&a, "chat"),
            None,
            "aborted block leaves no trace"
        );
        assert_eq!(a.root(), empty_root());
    }
}
