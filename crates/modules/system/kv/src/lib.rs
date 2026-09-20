//! qmdb-backed key-value module.
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: the HOST constructs
//! the concrete store (qmdb today — `statesync::qmdb::QmdbStore`) and hands it
//! to [`Kv::new`], so this crate never names a storage crate. the module's
//! authenticated [`StateRoot`] IS the store's merkle root — a real
//! cryptographic commitment to the whole store, refreshed on every commit — so
//! it flows directly into the global root-hash via `host::global_root`.
//!
//! ## keys are hashed to a fixed width
//!
//! the logical key is a `Vec<u8>` at the [`Module`]/interface seam, but the
//! store key is `sha256(logical_key)` — a fixed 32-byte digest. this is
//! deliberate and load-bearing: the store's state-sync resolvers are bounded on
//! fixed-width keys, and hashing is the canonical authenticated-KV pattern
//! (cf. `keccak(address)` in an eth state trie). the cost is that the store
//! commits to `hash(key) -> value` and cannot enumerate original keys — a
//! get/set KV never needs to.
//!
//! ## state-sync
//!
//! sync belongs to the injected store, not this module: a joiner (dynamic-valset
//! catch-up, a fresh full node, crash recovery) rebuilds the CONCRETE store from
//! a peer (`QmdbStore::sync_from`) and wraps a fresh `Kv` around it. this module
//! only forwards the trait's serve surface — [`Module::serve_sync`] and
//! [`Module::resolver_sync_target`] delegate straight to the store.

// the wire surface: this module's shared types, flattened at the crate root.
pub use kv_wire::*;

// the wasm-guest port: the dispatch shell that adapts this module to the
// ducktape:module world. compiled only by the guest-builder's synthesized
// wasm32 cdylib workspace (feature `guest`), never by the native build.
#[cfg(feature = "guest")]
mod guest;

use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, ResolverSyncTarget, StagedStore, StateRoot,
    StateSyncHandle,
};

/// write-time cap on a LOGICAL key. the store key is the 32-byte hash, so this is
/// a hygiene bound at the interface seam (an unbounded key would still bloat the
/// pending overlay and every message that carries it), not a storage-layout limit.
pub const MAX_KEY_LEN: usize = 4 * 1024;

/// write-time cap on a value. the concrete store's codec bounds a stored value
/// at 1 MiB AT DECODE TIME (see `statesync::qmdb::store_config`) — an oversized
/// value would COMMIT fine and then panic every later read (and any log replay /
/// sync batch decode) on every validator: a poison pill. rejecting here keeps it
/// out of the log entirely. the 4 KiB margin below the 1 MiB codec bound covers
/// the serialized operation's framing (32-byte hashed key, varint length prefix,
/// operation tag), so the WHOLE stored form — not just the raw value — stays
/// under the codec bound and under 1 MiB-scale wire-message caps when ops ship
/// in sync batches.
pub const MAX_VALUE_LEN: usize = (1 << 20) - 4 * 1024;

/// a qmdb-backed key-value module.
pub struct Kv {
    id: ModuleId,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes; folded into `root()` at `commit_block`). the store key
    /// is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl Kv {
    /// wrap the host-constructed store under module identity `id`. sync — the
    /// store arrives already opened (or already synced to a verified root).
    pub fn new(id: impl Into<ModuleId>, store: Box<dyn MerkleStore>) -> Self {
        Self {
            id: id.into(),
            staged: StagedStore::new(store),
        }
    }

    /// upsert `key -> value` as ONE committed batch. after this returns `root()`
    /// reflects the new committed merkle root. the store key is `sha256(key)`.
    /// a direct test/dev convenience — but it enforces the same write-time size
    /// caps as the consensus path (`execute` -> `stage`), so it can never commit
    /// the poison-pill value the caps exist to keep out. callers use it on an
    /// empty overlay; it stages then flushes, so its committed batch is the same
    /// single write.
    // ponytail: stage-then-commit flushes any co-staged overlay entry too; every
    // caller uses it on an empty overlay, so the batch is byte-identical to the
    // old direct single-entry commit.
    pub async fn set(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), Error> {
        self.stage(key, value)?;
        self.staged.commit().await
    }

    /// reject a write that would poison the store: its codec bound is enforced
    /// only at DECODE time, so an oversized value commits fine and then panics
    /// every later read of that key on EVERY validator. checked at write time
    /// (see [`MAX_KEY_LEN`] / [`MAX_VALUE_LEN`] for the cap rationale).
    fn check_write_caps(key: &[u8], value: &[u8]) -> Result<(), Error> {
        if key.len() > MAX_KEY_LEN {
            return Err(Error::module(
                "key_too_large",
                format!(
                    "key too large: {} bytes exceeds the {MAX_KEY_LEN}-byte cap",
                    key.len()
                ),
            ));
        }
        if value.len() > MAX_VALUE_LEN {
            return Err(Error::module(
                "value_too_large",
                format!(
                    "value too large: {} bytes exceeds the {MAX_VALUE_LEN}-byte cap",
                    value.len()
                ),
            ));
        }
        Ok(())
    }

    /// stage `key -> value` for this block WITHOUT committing. visible to `get`
    /// at once (read-your-writes) but folded into the store — and `root()` —
    /// only when the host calls `commit_block` at the block boundary. rejects an
    /// over-cap key/value BEFORE staging, so a failed op leaves no overlay entry.
    pub fn stage(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), Error> {
        Self::check_write_caps(&key, &value)?;
        self.staged.stage(key, value);
        Ok(())
    }

    /// read `key`: a STAGED (this-block) write shadows committed store state, so
    /// a later op in the same block sees an earlier staged write. committed reads
    /// go through the hashed key.
    pub async fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.staged.get(key).await.expect("get failed")
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Kv {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the REAL merkle root over all committed keys, as a 32-byte state root.
    /// sync, as the trait requires: the store caches its root and returns it by
    /// value. never a placeholder.
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

    /// interpret the payload as a json-encoded `(key, value)` write and apply it
    /// to own state. the only `.await` is on own store state — deterministic, so
    /// this is replay-safe across validators. an over-cap key/value is rejected
    /// here (write time), never staged, never committed — see [`MAX_VALUE_LEN`].
    async fn execute(&mut self, _ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        match crate::decode(&msg.payload).map_err(|e| Error::module("codec", e))? {
            crate::KvMsg::Set { key, value } => self.stage(key, value),
        }
    }

    /// real async read of own store state — the async-query seam in action.
    /// serves STAGED-over-committed via `get`, so cross-module reads within a
    /// block observe this block's staged writes.
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match crate::decode_query(req).map_err(|e| Error::module("codec", e))? {
            crate::KvQuery::Get { key } => Ok(crate::encode_reply(&crate::KvReply::Value(
                self.get(&key).await,
            ))),
        }
    }

    /// publish the block's staged writes in ONE store batch. no-op (and no root
    /// movement) if nothing was staged. a single-write block issues the exact
    /// same batch `set` did, so its committed root is byte-identical to the
    /// per-op path. kv only ever stages full values (there is no delete op), so
    /// every entry ships as `Some`; BTreeMap iteration keeps the write order
    /// deterministic across validators.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    /// discard the block's staged writes — nothing reached the store, so
    /// `root()` is unchanged.
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
    use host::global_root;
    use sdk_testkit::TestCtx;
    use statesync::qmdb::QmdbStore;

    // a fixed-root stand-in module, so we can prove the kv root composes into the
    // global root-hash alongside another module.
    struct StubModule;
    #[async_trait::async_trait(?Send)]
    impl Module for StubModule {
        fn id(&self) -> ModuleId {
            "stub".to_string()
        }
        fn root(&self) -> StateRoot {
            StateRoot([7u8; sdk::ROOT_LEN])
        }
        async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
            Ok(())
        }
    }

    // build the module the way a host does: concrete store first, injected as
    // `Box<dyn MerkleStore>`. a macro (not an fn) so the tests need no
    // dev-dependency on commonware-storage just to spell the context bounds.
    macro_rules! kv_on {
        ($context:expr, $id:expr) => {
            Kv::new($id, Box::new(QmdbStore::init($context, $id).await))
        };
    }

    #[test]
    fn real_qmdb_root_flows_into_root_hash() {
        deterministic::Runner::default().start(|context| async move {
            let mut kv = kv_on!(context, "kv");
            let stub = StubModule;

            let r0 = kv.root();

            kv.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
            let r1 = kv.root();
            let app1 = {
                let mods: [&dyn Module; 2] = [&kv, &stub];
                global_root(&mods)
            };

            kv.set(b"k2".to_vec(), b"v2".to_vec()).await.expect("set");
            let r2 = kv.root();
            let app2 = {
                let mods: [&dyn Module; 2] = [&kv, &stub];
                global_root(&mods)
            };

            // every write moves the real merkle root, and the post-write roots are
            // genuine (never the zero placeholder).
            assert_ne!(r0, r1, "first write must move the root");
            assert_ne!(r1, r2, "second write must move the root");
            assert_ne!(r1, StateRoot::ZERO, "root after write must be non-zero");
            assert_ne!(r2, StateRoot::ZERO, "root after write must be non-zero");

            // values round-trip through the store.
            assert_eq!(kv.get(b"k1").await.as_deref(), Some(b"v1".as_ref()));
            assert_eq!(kv.get(b"k2").await.as_deref(), Some(b"v2".as_ref()));

            // the kv merkle root genuinely flows into the composed root-hash: only
            // kv changed between r1 and r2, yet the global root differs.
            assert_ne!(
                app1, app2,
                "mutating only the kv module must change the global root-hash"
            );
        });
    }

    // robustness guard: the qmdb read/write/merkle path must survive EVERY task
    // schedule the deterministic runtime can produce — that is exactly the
    // property a consensus state machine needs (each validator schedules
    // differently). a lost write under any seed would be a real ordering bug.
    #[test]
    fn no_lost_writes_across_schedules() {
        let mut fails: Vec<u64> = Vec::new();
        for seed in 0..64u64 {
            let ok = deterministic::Runner::seeded(seed).start(|context| async move {
                let mut kv = kv_on!(context, "kv");
                kv.set(b"k1".to_vec(), b"v1".to_vec()).await.expect("set");
                let g1 = kv.get(b"k1").await;
                kv.set(b"k2".to_vec(), b"v2".to_vec()).await.expect("set");
                let g2 = kv.get(b"k2").await;
                g1.as_deref() == Some(b"v1".as_ref())
                    && g2.as_deref() == Some(b"v2".as_ref())
                    && kv.root() != StateRoot::ZERO
            });
            if !ok {
                fails.push(seed);
            }
        }
        assert!(fails.is_empty(), "lost write / None on seeds: {:?}", fails);
    }

    // the poison-pill guard: an over-cap set is rejected at WRITE time — never
    // staged, never committed, root unchanged — instead of committing fine and
    // panicking every later decode of that key on every validator.
    #[test]
    fn oversized_writes_are_rejected_before_staging() {
        deterministic::Runner::default().start(|context| async move {
            let mut kv = kv_on!(context, "kv");
            let r0 = kv.root();

            // value one byte over the cap -> rejected, nothing staged.
            let huge_value = crate::encode(&crate::KvMsg::Set {
                key: b"k".to_vec(),
                value: vec![0u8; MAX_VALUE_LEN + 1],
            });
            let err = kv
                .execute(
                    &mut TestCtx::at_height(0),
                    &Msg {
                        target: "kv".into(),
                        payload: huge_value,
                    },
                )
                .await
                .expect_err("over-cap value must be rejected");
            assert!(
                matches!(err, Error::Module { ref reason, .. } if reason == "value_too_large"),
                "unexpected error: {err:?}"
            );

            // key one byte over the cap -> rejected, nothing staged.
            let huge_key = crate::encode(&crate::KvMsg::Set {
                key: vec![b'k'; MAX_KEY_LEN + 1],
                value: b"v".to_vec(),
            });
            let err = kv
                .execute(
                    &mut TestCtx::at_height(0),
                    &Msg {
                        target: "kv".into(),
                        payload: huge_key,
                    },
                )
                .await
                .expect_err("over-cap key must be rejected");
            assert!(
                matches!(err, Error::Module { ref reason, .. } if reason == "key_too_large"),
                "unexpected error: {err:?}"
            );

            // the rejects happened BEFORE staging: no overlay entry, and a commit
            // is a no-op that leaves the root byte-identical.
            assert!(kv.staged.is_empty(), "a rejected write must not be staged");
            kv.commit_block().await.expect("commit");
            assert_eq!(kv.root(), r0, "a rejected write must not move the root");

            // the direct `set` convenience enforces the same caps — it must
            // never commit a poison-pill value either.
            let err = kv
                .set(b"k".to_vec(), vec![0u8; MAX_VALUE_LEN + 1])
                .await
                .expect_err("over-cap set must be rejected");
            assert!(
                matches!(err, Error::Module { ref reason, .. } if reason == "value_too_large"),
                "unexpected error: {err:?}"
            );
            assert_eq!(kv.root(), r0, "a rejected set must not move the root");

            // boundary: exactly-at-cap writes are accepted and commit fine.
            kv.stage(vec![b'k'; MAX_KEY_LEN], vec![0u8; MAX_VALUE_LEN])
                .expect("at-cap write");
            kv.commit_block().await.expect("commit at-cap write");
            assert_eq!(
                kv.get(&vec![b'k'; MAX_KEY_LEN]).await.map(|v| v.len()),
                Some(MAX_VALUE_LEN)
            );
        });
    }

    // isolation: two qmdb modules on ONE runtime context must not share storage.
    // same key written to each stays independent, and the roots diverge.
    #[test]
    fn two_modules_on_one_context_dont_collide() {
        deterministic::Runner::default().start(|context| async move {
            let mut a = kv_on!(context.child("alpha"), "alpha");
            let mut b = kv_on!(context.child("beta"), "beta");
            a.set(b"x".to_vec(), b"1".to_vec()).await.expect("set");
            b.set(b"x".to_vec(), b"2".to_vec()).await.expect("set");
            assert_eq!(a.get(b"x").await.as_deref(), Some(b"1".as_ref()));
            assert_eq!(b.get(b"x").await.as_deref(), Some(b"2".as_ref()));
            assert_ne!(
                a.root(),
                b.root(),
                "isolated modules must have distinct roots"
            );
        });
    }
}
