//! the batch-apply API: [`Host::submit_block`] applies a batch of ops as ONE
//! block with per-op isolation, a SINGLE commit boundary, and ONE post-batch
//! root-hash shared by every applied member. four properties:
//!
//! 1. per-op isolation: a batch `[A, <reject>, C]` applies A and C and rejects
//!    the middle op, landing committed state byte-identical to applying A then C
//!    in isolation, under ONE shared post-batch root-hash;
//! 2. an all-reject batch commits nothing — root-hash unchanged from pre-batch;
//! 3. a batch-of-one equals `submit_at` of that op (same root-hash + roots);
//! 4. an empty batch is an empty block — root-hash unchanged when nothing pending.
//!
//! the `directory` module is an in-memory, content-addressed, staging module
//! (stage on `execute`, merge on `commit_block`, discard on `abort_block`), so a
//! `DirMsg::Set` is a deterministically-APPLYING op. an op whose target is not
//! registered fails the drain on the remove with `Error::UnknownModule` — a
//! deterministically-REJECTING op (the same shape the node's heartbeat nop uses).

use std::cell::Cell;
use std::rc::Rc;

use directory::{DirMsg, Directory, decode_msg, encode_msg as dir_encode};
use futures::executor::block_on;
use host::{BlockContext, Host, MemberOutcome};
use sdk::{Module, Msg, Origin};

const DIR: &str = "directory";

fn set(key: &str, value: &str) -> Msg {
    Msg {
        target: DIR.into(),
        payload: dir_encode(&DirMsg::Set {
            key: key.into(),
            value: value.into(),
        }),
    }
}

// an op targeting a module that isn't registered: the drain fails on the remove
// with `Error::UnknownModule` — a DETERMINISTIC rejection.
fn reject() -> Msg {
    Msg {
        target: "no-such-module".into(),
        payload: Vec::new(),
    }
}

fn ext() -> Origin {
    Origin::External(Vec::new())
}

fn host() -> Host {
    Host::genesis(vec![Box::new(Directory::new(DIR))]).expect("genesis")
}

// (1) a middle op that rejects deterministically leaves the committed state
// identical to applying only the surviving ops, in order, in isolation — under
// exactly one shared post-batch root-hash.
#[test]
fn submit_block_per_op_isolation() {
    block_on(async {
        let mut batch = host();
        let out = batch
            .submit_block(
                BlockContext::default(),
                vec![
                    (ext(), set("a", "1")),
                    (ext(), reject()),
                    (ext(), set("b", "2")),
                ],
            )
            .await
            .expect("the batch itself applies; per-op verdicts live in members");

        // members mirror input order: applied, rejected, applied.
        assert_eq!(out.members.len(), 3);
        assert!(
            matches!(out.members[0], MemberOutcome::Applied { .. }),
            "op0 applies"
        );
        assert!(
            matches!(out.members[1], MemberOutcome::Rejected { .. }),
            "op1 rejects deterministically"
        );
        assert!(
            matches!(out.members[2], MemberOutcome::Applied { .. }),
            "op2 applies"
        );

        // reference: submit_at(Set a=1) then submit_at(Set b=2), each in isolation.
        let mut reference = host();
        reference
            .submit_at(BlockContext::default(), set("a", "1"))
            .await
            .expect("apply a");
        let ref_out = reference
            .submit_at(BlockContext::default(), set("b", "2"))
            .await
            .expect("apply b");

        // every module root is byte-identical to the isolated-application reference.
        assert_eq!(
            batch.module_root(DIR).unwrap(),
            reference.module_root(DIR).unwrap(),
            "batch directory root must equal applying a then b in isolation"
        );
        // exactly one post-batch root-hash, equal to the reference's, recompute-stable.
        assert_eq!(
            out.root_hash, ref_out.root_hash,
            "the one batch root-hash equals the reference root-hash"
        );
        assert_eq!(batch.root_hash(), reference.root_hash());
        assert_eq!(
            out.root_hash,
            batch.root_hash(),
            "the returned root-hash is recompute-stable"
        );
    });
}

// (2) an all-reject batch commits nothing — every member rejects and the
// root-hash is byte-identical to pre-batch.
#[test]
fn submit_block_all_reject() {
    block_on(async {
        let mut host = host();
        let app0 = host.root_hash();

        let out = host
            .submit_block(
                BlockContext::default(),
                vec![(ext(), reject()), (ext(), reject()), (ext(), reject())],
            )
            .await
            .expect("an all-reject batch still commits an (empty) block");

        assert_eq!(out.members.len(), 3);
        assert!(
            out.members
                .iter()
                .all(|m| matches!(m, MemberOutcome::Rejected { .. })),
            "every member rejected"
        );
        assert_eq!(
            out.root_hash, app0,
            "no member applied — root-hash unchanged from pre-batch"
        );
        assert_eq!(host.root_hash(), app0);
    });
}

// (3) the batch-of-one path equals the single-op path: same root-hash and same
// committed roots as `submit_at` of that op.
#[test]
fn submit_block_single_matches_submit_at() {
    block_on(async {
        let mut batch = host();
        let out = batch
            .submit_block(BlockContext::default(), vec![(ext(), set("k", "v"))])
            .await
            .expect("batch-of-one applies");
        assert_eq!(out.members.len(), 1);
        assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));

        let mut single = host();
        let single_out = single
            .submit_at(BlockContext::default(), set("k", "v"))
            .await
            .expect("single op applies");

        assert_eq!(
            out.root_hash, single_out.root_hash,
            "batch-of-one root-hash == submit_at root-hash"
        );
        assert_eq!(
            batch.module_root(DIR).unwrap(),
            single.module_root(DIR).unwrap(),
            "batch-of-one committed root == submit_at committed root"
        );
        assert_eq!(batch.root_hash(), single.root_hash());
    });
}

// (4) an empty batch is an empty block: no members, and with nothing pending to
// inject the root-hash is unchanged.
#[test]
fn submit_block_empty_is_empty_block() {
    block_on(async {
        let mut host = host();
        let app0 = host.root_hash();

        let out = host
            .submit_block(BlockContext::default(), vec![])
            .await
            .expect("an empty batch commits an empty block");

        assert!(out.members.is_empty(), "no ops -> no members");
        assert!(out.events.is_empty());
        assert!(
            out.system_dispatches.is_empty(),
            "no upgrade/dispatch modules -> nothing injected"
        );
        assert_eq!(
            out.root_hash, app0,
            "no ops, no injections — root-hash unchanged (an empty block)"
        );
        assert_eq!(host.root_hash(), app0);
    });
}

// ---------------------------------------------------------------------------
// the COST of isolation: a rejection that staged nothing must not roll back and
// replay the members before it, and the replays a staging rejection does cost
// are bounded per block.

/// a `Directory` that counts its `execute` calls and, on the key `fail`, STAGES
/// a write and then rejects — the rejection class whose partial stage really is
/// entangled with the accepted members' (unlike an unknown target, which never
/// reaches a module at all).
struct Counting {
    inner: Directory,
    executes: Rc<Cell<u32>>,
}

#[async_trait::async_trait(?Send)]
impl Module for Counting {
    fn id(&self) -> sdk::ModuleId {
        self.inner.id()
    }

    fn root(&self) -> sdk::StateRoot {
        self.inner.root()
    }

    fn state_sync_handle(&self) -> Result<sdk::StateSyncHandle, sdk::Error> {
        self.inner.state_sync_handle()
    }

    async fn execute(&mut self, ctx: &mut dyn sdk::Ctx, msg: &Msg) -> Result<(), sdk::Error> {
        self.executes.set(self.executes.get() + 1);
        self.inner.execute(ctx, msg).await?;
        match decode_msg(&msg.payload) {
            Ok(DirMsg::Set { key, .. }) if key == "fail" => Err(sdk::Error::module(
                "staged_then_rejected",
                "staged, then rejected",
            )),
            _ => Ok(()),
        }
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, sdk::Error> {
        self.inner.query(req).await
    }

    async fn commit_block(&mut self) -> Result<(), sdk::Error> {
        self.inner.commit_block().await
    }

    async fn abort_block(&mut self) -> Result<(), sdk::Error> {
        self.inner.abort_block().await
    }
}

/// a host whose one module counts executions; returns the shared counter.
fn counting_host() -> (Host, Rc<Cell<u32>>) {
    let executes = Rc::new(Cell::new(0));
    let module = Counting {
        inner: Directory::new(DIR),
        executes: Rc::clone(&executes),
    };
    (
        Host::genesis(vec![Box::new(module)]).expect("genesis"),
        executes,
    )
}

async fn committed(host: &Host, key: &str) -> Option<String> {
    let req = directory::encode_query(&directory::DirQuery::Get { key: key.into() });
    let bytes = host.query(DIR, &req).await.expect("query");
    match directory::decode_reply(&bytes).expect("decode") {
        directory::DirReply::Value(v) => v,
    }
}

// a batch of [applies, unknown-target] * N executes each applied op EXACTLY
// ONCE: an unknown target is rejected before any module is reached, so it has
// nothing to roll back and nothing to replay. rolling back regardless made this
// quadratic (~N²/2 executions) — one 1 MiB frame of alternating members stalled
// every validator.
#[test]
fn a_rejection_that_staged_nothing_replays_nothing() {
    block_on(async {
        const N: usize = 16;
        let (mut host, executes) = counting_host();
        let mut ops = Vec::new();
        for i in 0..N {
            ops.push((ext(), set(&format!("k{i}"), "v")));
            ops.push((ext(), reject()));
        }

        let out = host
            .submit_block(BlockContext::default(), ops)
            .await
            .expect("the batch applies");

        assert_eq!(out.members.len(), 2 * N);
        assert_eq!(
            executes.get(),
            N as u32,
            "each applied member executes exactly once — no replay",
        );
        for i in 0..N {
            assert!(matches!(out.members[2 * i], MemberOutcome::Applied { .. }));
            assert!(matches!(
                out.members[2 * i + 1],
                MemberOutcome::Rejected { .. }
            ));
            assert_eq!(committed(&host, &format!("k{i}")).await, Some("v".into()));
        }
    });
}

// a member that STAGED and then failed is still rolled back and the accepted
// members replayed — the batch commits exactly the accepted subset, and the
// failed member's staged write is not in it.
#[test]
fn a_staged_then_failing_member_commits_the_accepted_subset() {
    block_on(async {
        let (mut host, executes) = counting_host();
        let out = host
            .submit_block(
                BlockContext::default(),
                vec![
                    (ext(), set("a", "1")),
                    (ext(), set("fail", "x")),
                    (ext(), set("b", "2")),
                ],
            )
            .await
            .expect("the batch applies");

        assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));
        assert!(matches!(out.members[1], MemberOutcome::Rejected { .. }));
        assert!(matches!(out.members[2], MemberOutcome::Applied { .. }));
        assert_eq!(committed(&host, "a").await, Some("1".into()));
        assert_eq!(committed(&host, "b").await, Some("2".into()));
        assert_eq!(
            committed(&host, "fail").await,
            None,
            "the rejected member's staged write never commits"
        );
        // a=1, fail, b=2, plus the ONE replay of `a` the rollback owes.
        assert_eq!(executes.get(), 4, "exactly one rollback+replay cycle");
    });
}

