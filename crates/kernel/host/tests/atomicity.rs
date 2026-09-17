//! per-block atomicity: the HOST owns the commit lifecycle. modules STAGE their
//! writes during the drain; the host commits every touched module together at
//! the block boundary on success, or aborts (discards staged writes) on any
//! failure. three properties:
//!
//! (a) a block whose drain FAILS partway leaves EVERY module's root unchanged —
//!     full rollback, no trace (`root_hash` byte-identical to pre-block);
//! (b) a successful multi-write block commits ALL writes together (the root-hash
//!     reflects them, and is recompute-stable);
//! (c) read-your-writes: a later op in the SAME block sees an earlier staged
//!     write (through a cross-module `ctx.query`), before any commit.

use commonware_runtime::{Runner as _, deterministic};
use directory::Directory;
use directory::{DirMsg, DirQuery, DirReply, decode_reply, encode_msg as dir_encode, encode_query};
use host::Host;
use sdk::{Ctx, Error, Event, Module, ModuleId, Msg, StateRoot};
use statesync::qmdb::QmdbStore;

const DIR: &str = "directory";
const KV: &str = "kv";

fn kv_set(key: &str, value: &str) -> Vec<u8> {
    kv::encode(&kv::KvMsg::Set {
        key: key.as_bytes().to_vec(),
        value: value.as_bytes().to_vec(),
    })
}

// a module whose execute ALWAYS fails — the forced mid-block failure.
struct Boom;
#[async_trait::async_trait(?Send)]
impl Module for Boom {
    fn id(&self) -> ModuleId {
        "boom".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, _c: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        Err(Error::module("boom", "boom"))
    }
}

// fans a single root op into follow-up writes to directory + kv, and — when
// `fail` — a trailing op to `boom`, so the block errors AFTER those writes have
// already staged. the writes and the failure land in ONE block.
struct Fanout {
    fail: bool,
}
#[async_trait::async_trait(?Send)]
impl Module for Fanout {
    fn id(&self) -> ModuleId {
        "fanout".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        ctx.emit_msg(Msg {
            target: DIR.into(),
            payload: dir_encode(&DirMsg::Set {
                key: "k".into(),
                value: "v".into(),
            }),
        });
        ctx.emit_msg(Msg {
            target: KV.into(),
            payload: kv_set("k", "v"),
        });
        if self.fail {
            ctx.emit_msg(Msg {
                target: "boom".into(),
                payload: Vec::new(),
            });
        }
        Ok(())
    }
}

// (a) a failed block rolls back EVERY module — no root moves, no trace.
#[test]
fn failed_block_rolls_back_every_module() {
    deterministic::Runner::default().start(|context| async move {
        let kv = kv::Kv::new(KV, Box::new(QmdbStore::init(context, KV).await));
        let mut host = Host::genesis(vec![
            Box::new(kv),
            Box::new(Directory::new(DIR)),
            Box::new(Fanout { fail: true }),
            Box::new(Boom),
        ])
        .expect("genesis");

        let dir0 = host.module_root(DIR).unwrap();
        let kv0 = host.module_root(KV).unwrap();
        let app0 = host.root_hash();

        let err = host
            .submit(Msg {
                target: "fanout".into(),
                payload: Vec::new(),
            })
            .await
            .expect_err("the boom follow-up must fail the block");
        assert_eq!(
            err,
            host::SubmitError::Rejected(Error::module("boom", "boom"))
        );

        // no trace: every root and the root-hash are byte-identical to pre-block.
        assert_eq!(
            host.module_root(DIR).unwrap(),
            dir0,
            "directory must roll back"
        );
        assert_eq!(host.module_root(KV).unwrap(), kv0, "kv must roll back");
        assert_eq!(
            host.root_hash(),
            app0,
            "root-hash must be unchanged after a failed block"
        );

        // the staged value is truly gone — not merely root-invisible.
        let r = host
            .query(DIR, &encode_query(&DirQuery::Get { key: "k".into() }))
            .await
            .unwrap();
        assert_eq!(
            decode_reply(&r).unwrap(),
            DirReply::Value(None),
            "staged write must be discarded"
        );
    });
}

// (a) budget exhaustion is a drain failure too: a self-emitting module that also
// staged a real write must roll that write back when it hits MAX_DISPATCHES.
struct Looper;
#[async_trait::async_trait(?Send)]
impl Module for Looper {
    fn id(&self) -> ModuleId {
        "looper".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, _m: &Msg) -> Result<(), Error> {
        // stage a directory write, then re-emit to self forever -> BudgetExceeded.
        ctx.emit_msg(Msg {
            target: DIR.into(),
            payload: dir_encode(&DirMsg::Set {
                key: "k".into(),
                value: "v".into(),
            }),
        });
        ctx.emit_msg(Msg {
            target: "looper".into(),
            payload: Vec::new(),
        });
        Ok(())
    }
}

#[test]
fn budget_exceeded_also_rolls_back() {
    deterministic::Runner::default().start(|_| async move {
        let mut host =
            Host::genesis(vec![Box::new(Directory::new(DIR)), Box::new(Looper)]).expect("genesis");

        let dir0 = host.module_root(DIR).unwrap();
        let app0 = host.root_hash();

        let err = host
            .submit(Msg {
                target: "looper".into(),
                payload: Vec::new(),
            })
            .await
            .expect_err("must hit the dispatch budget");
        assert_eq!(err, host::SubmitError::Rejected(Error::BudgetExceeded));

        assert_eq!(
            host.module_root(DIR).unwrap(),
            dir0,
            "directory must roll back on budget exhaustion"
        );
        assert_eq!(
            host.root_hash(),
            app0,
            "root-hash unchanged after a budget-exceeded block"
        );
    });
}

// (b) a successful multi-write block commits ALL writes together.
#[test]
fn successful_multi_write_block_commits_all_together() {
    deterministic::Runner::default().start(|context| async move {
        let kv = kv::Kv::new(KV, Box::new(QmdbStore::init(context, KV).await));
        let mut host = Host::genesis(vec![
            Box::new(kv),
            Box::new(Directory::new(DIR)),
            Box::new(Fanout { fail: false }),
        ])
        .expect("genesis");

        let dir0 = host.module_root(DIR).unwrap();
        let kv0 = host.module_root(KV).unwrap();
        let app0 = host.root_hash();

        let out = host
            .submit(Msg {
                target: "fanout".into(),
                payload: Vec::new(),
            })
            .await
            .expect("clean block must succeed");

        // both writes landed at the boundary — both roots moved.
        assert_ne!(
            host.module_root(DIR).unwrap(),
            dir0,
            "directory must commit"
        );
        assert_ne!(host.module_root(KV).unwrap(), kv0, "kv must commit");
        assert_ne!(
            out.root_hash, app0,
            "root-hash must reflect the committed writes"
        );
        assert_eq!(
            out.root_hash,
            host.root_hash(),
            "root-hash must be recompute-stable"
        );

        // and the values are readable post-commit.
        let r = host
            .query(DIR, &encode_query(&DirQuery::Get { key: "k".into() }))
            .await
            .unwrap();
        assert_eq!(decode_reply(&r).unwrap(), DirReply::Value(Some("v".into())));
    });
}

// (c) read-your-writes: an op emits a directory write, then a follow-up op reads
// the SAME key back — through the host-routed cross-module query — and must see
// the staged value, before the block has committed.
struct RywProbe;
#[async_trait::async_trait(?Send)]
impl Module for RywProbe {
    fn id(&self) -> ModuleId {
        "ryw".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        match msg.payload.as_slice() {
            b"start" => {
                // stage a directory write, THEN queue a self-op to read it back
                // within the same block. FIFO guarantees the write dispatches first.
                ctx.emit_msg(Msg {
                    target: DIR.into(),
                    payload: dir_encode(&DirMsg::Set {
                        key: "ryw".into(),
                        value: "staged".into(),
                    }),
                });
                ctx.emit_msg(Msg {
                    target: "ryw".into(),
                    payload: b"verify".to_vec(),
                });
                Ok(())
            }
            b"verify" => {
                let reply = ctx
                    .query(DIR, &encode_query(&DirQuery::Get { key: "ryw".into() }))
                    .await?;
                let seen = match decode_reply(&reply).map_err(|e| Error::module("codec", e))? {
                    DirReply::Value(Some(v)) => v == "staged",
                    _ => false,
                };
                ctx.emit_event(Event {
                    source: "ryw".into(),
                    payload: if seen {
                        b"SAW".to_vec()
                    } else {
                        b"MISS".to_vec()
                    },
                });
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

// (c) read-your-writes against the QMDB kv module specifically: staging goes to
// an in-memory pending map read AHEAD of committed qmdb state (never a read from
// an uncommitted qmdb batch), so a later op sees the staged write pre-commit.
struct KvRywProbe;
#[async_trait::async_trait(?Send)]
impl Module for KvRywProbe {
    fn id(&self) -> ModuleId {
        "kvryw".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        match msg.payload.as_slice() {
            b"start" => {
                ctx.emit_msg(Msg {
                    target: KV.into(),
                    payload: kv_set("kryw", "staged"),
                });
                ctx.emit_msg(Msg {
                    target: "kvryw".into(),
                    payload: b"verify".to_vec(),
                });
                Ok(())
            }
            b"verify" => {
                let reply = ctx
                    .query(
                        KV,
                        &kv::encode_query(&kv::KvQuery::Get {
                            key: b"kryw".to_vec(),
                        }),
                    )
                    .await?;
                let seen = matches!(
                    kv::decode_reply(&reply).map_err(|e| Error::module("codec", e))?,
                    kv::KvReply::Value(Some(v)) if v == b"staged"
                );
                ctx.emit_event(Event {
                    source: "kvryw".into(),
                    payload: if seen {
                        b"SAW".to_vec()
                    } else {
                        b"MISS".to_vec()
                    },
                });
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[test]
fn read_your_writes_against_qmdb_within_a_block() {
    deterministic::Runner::default().start(|context| async move {
        let kv = kv::Kv::new(KV, Box::new(QmdbStore::init(context, KV).await));
        let mut host = Host::genesis(vec![Box::new(kv), Box::new(KvRywProbe)]).expect("genesis");

        let kv0 = host.module_root(KV).unwrap();
        let out = host
            .submit(Msg {
                target: "kvryw".into(),
                payload: b"start".to_vec(),
            })
            .await
            .expect("clean block");

        assert!(
            out.events.iter().any(|e| e.payload == b"SAW"),
            "a later op must see an earlier staged QMDB write before commit"
        );
        // and it did commit at the boundary — the qmdb root moved.
        assert_ne!(
            host.module_root(KV).unwrap(),
            kv0,
            "kv must commit at the boundary"
        );
    });
}

#[test]
fn read_your_writes_within_a_block() {
    deterministic::Runner::default().start(|_| async move {
        let mut host = Host::genesis(vec![Box::new(Directory::new(DIR)), Box::new(RywProbe)])
            .expect("genesis");

        let out = host
            .submit(Msg {
                target: "ryw".into(),
                payload: b"start".to_vec(),
            })
            .await
            .expect("clean block");

        // the verify op observed the write staged EARLIER in the same block.
        assert!(
            out.events.iter().any(|e| e.payload == b"SAW"),
            "a later op must see an earlier staged write (read-your-writes)"
        );
    });
}
