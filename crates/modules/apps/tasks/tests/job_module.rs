//! the job board under test (via the merged `tasks` work module): the full
//! lifecycle, every race/guard rejection, caps, lease clamping, the `Get`
//! point read, origin-derived identity, and commit/abort staging -- plus
//! real-`Host` proofs that first-claim-wins under the host's ordered dispatch.
//! board ENUMERATION (status/kind listings, the census) is the index tier's
//! job now, covered by the native tests in `src/index.rs`.
//!
//! the board lives inside the `tasks` module now, so ops ride the `WorkMsg`
//! envelope (`encode_job_*`), and it is qmdb-backed: one record per job over
//! the injected store. these tests inject an in-memory store and assert
//! BEHAVIOR; the cross-node round trip over the REAL store is `sync_round_trip`.

use futures::executor::block_on;
use host::{BlockContext, Host, SubmitError};
use sdk::{Ctx, Env, Error, MerkleStore as _, Module, ModuleId, Msg, Origin, StateRoot};
use sdk_testkit::{MemStore, TestCtx};
use tasks::{
    Job, JobStatus, JobsEvent, JobsMsg, JobsQuery, JobsReply,
    decode_job_event as decode_jobs_event, decode_job_reply as decode_reply,
    encode_job_event as encode_jobs_event, encode_job_msg as encode_msg,
    encode_job_query as encode_query,
};
use tasks::{
    MAX_ATTEMPTS, MAX_JOBS, MAX_KIND, MAX_LIVE_JOBS_PER_SUBMITTER, MAX_PAYLOAD, MAX_SPEC,
    MAX_WORKERS, Tasks as Jobs,
};

// the merged work module's genesis id -- the job board now lives here.
const JOBS: &str = "tasks";

/// build the module the way a host does: concrete store first, injected as
/// `Box<dyn MerkleStore>`. these tests assert BEHAVIOR, so the in-memory store
/// stands in for qmdb; the real-store round trip lives in `sync_round_trip`.
fn jobs_on_mem() -> Jobs {
    Jobs::new(JOBS, "identity", "attribution", Box::new(MemStore::new()))
}

// ---- wire builders ---------------------------------------------------------

fn jobs_msg(m: JobsMsg) -> Msg {
    Msg {
        target: JOBS.into(),
        payload: encode_msg(&m),
    }
}

fn submit(job_id: &str, kind: &str, spec: &str) -> Msg {
    jobs_msg(JobsMsg::Submit {
        job_id: job_id.into(),
        kind: kind.into(),
        spec: spec.into(),
    })
}

fn claim(job_id: &str, lease_views: u64) -> Msg {
    jobs_msg(JobsMsg::Claim {
        job_id: job_id.into(),
        lease_views,
    })
}

fn finalize(job_id: &str, ok: bool, payload: &str) -> Msg {
    jobs_msg(JobsMsg::Finalize {
        job_id: job_id.into(),
        ok,
        payload: payload.into(),
    })
}

fn release(job_id: &str) -> Msg {
    jobs_msg(JobsMsg::Release {
        job_id: job_id.into(),
    })
}

fn reclaim(job_id: &str) -> Msg {
    jobs_msg(JobsMsg::Reclaim {
        job_id: job_id.into(),
    })
}

fn cancel(job_id: &str) -> Msg {
    jobs_msg(JobsMsg::Cancel {
        job_id: job_id.into(),
    })
}

fn prune(job_id: &str) -> Msg {
    jobs_msg(JobsMsg::Prune {
        job_id: job_id.into(),
    })
}

fn register_worker() -> Msg {
    jobs_msg(JobsMsg::RegisterWorker {})
}

fn unregister_worker() -> Msg {
    jobs_msg(JobsMsg::UnregisterWorker {})
}

fn ext(id: &str) -> Origin {
    Origin::External(id.as_bytes().to_vec())
}

/// the module's own external-actor derivation, mirrored so tests can name the
/// exact worker/submitter string an external origin produces: `ext:` +
/// lowercase hex (domain-separated from module ids and "system").
fn actor(id: &str) -> tasks::Party {
    tasks::Party::Key(id.as_bytes().to_vec())
}

// ---- a configurable dispatch ctx (height + origin) -------------------------
// jobs never reads module_root, so no live-module set is needed here; the
// shared TestCtx stands in behind a thin constructor.
fn ctx(height: u64, origin: Origin) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: 0,
        origin,
        me: JOBS.into(),
        cause: sdk::Cause::Direct,
    })
    .on_query("identity", |_| {
        Ok(identity::encode_reply(&identity::IdentityReply::Account(
            None,
        )))
    })
}

/// Supply the committed Identity account at the module query boundary. The
/// dispatch origin still decides which key or program is actually acting.
fn account_ctx(height: u64, origin: Origin, account: &identity::AccountView) -> TestCtx {
    let account = account.clone();
    ctx(height, origin).on_query("identity", move |_| {
        Ok(identity::encode_reply(&identity::IdentityReply::Account(
            Some(account.clone()),
        )))
    })
}

fn key_account() -> identity::AccountView {
    identity::AccountView {
        number: 1,
        name: "Submitter".into(),
        control: identity::Control::Keys,
        keys: ["founder", "sibling"]
            .into_iter()
            .map(|key| identity::KeyView {
                scheme: identity::KeyScheme::Ed25519,
                pubkey: key.as_bytes().to_vec(),
                label: None,
                added_at: 1,
            })
            .collect(),
        avatar: None,
        bio: None,
        updated_at: 1,
    }
}

// ---- test helpers ----------------------------------------------------------

/// execute one op at a height and origin, then commit it as its own block.
async fn apply(jobs: &mut Jobs, height: u64, origin: Origin, msg: Msg) {
    jobs.execute(&mut ctx(height, origin), &msg)
        .await
        .expect("op should apply");
    jobs.commit_block().await.expect("commit");
}

/// execute one op WITHOUT committing (leaves it staged).
async fn stage(jobs: &mut Jobs, height: u64, origin: Origin, msg: Msg) -> Result<(), Error> {
    jobs.execute(&mut ctx(height, origin), &msg).await
}

async fn get(jobs: &Jobs, job_id: &str) -> Option<Job> {
    let JobsReply::Job(job) = decode_reply(
        &jobs
            .query(&encode_query(&JobsQuery::Get {
                job_id: job_id.into(),
            }))
            .await
            .expect("query get"),
    )
    .expect("decode") else {
        panic!("expected job reply")
    };
    job
}

// ============================================================================
// lifecycle
// ============================================================================

#[test]
fn full_lifecycle_happy_path() {
    block_on(async {
        let mut jobs = jobs_on_mem();

        apply(
            &mut jobs,
            1,
            ext("submitter"),
            submit("j1", "email", "spec-body"),
        )
        .await;
        let job = get(&jobs, "j1").await.expect("exists");
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.attempt, 0);
        assert_eq!(job.submitter, actor("submitter"));
        assert!(job.claim.is_none());
        assert_eq!(job.created_at_height, 1);
        assert_eq!(job.updated_at_height, 1);

        apply(&mut jobs, 2, ext("worker-a"), claim("j1", 50)).await;
        let job = get(&jobs, "j1").await.expect("exists");
        assert_eq!(job.status, JobStatus::Processing);
        assert_eq!(job.attempt, 1);
        let c = job.claim.as_ref().expect("claim");
        assert_eq!(c.worker, actor("worker-a"));
        assert_eq!(c.claimed_at_height, 2);
        assert_eq!(c.lease_views, 50);
        assert_eq!(job.updated_at_height, 2);

        apply(
            &mut jobs,
            3,
            ext("worker-a"),
            finalize("j1", true, "done-payload"),
        )
        .await;
        let job = get(&jobs, "j1").await.expect("exists");
        assert_eq!(job.status, JobStatus::Done);
        let r = job.result.as_ref().expect("result");
        assert!(r.ok);
        assert_eq!(r.payload, "done-payload");
        // the claim is retained for the record.
        assert_eq!(job.claim.as_ref().unwrap().worker, actor("worker-a"));
        assert_eq!(job.updated_at_height, 3);
    });
}

// ============================================================================
// worker registry + event codec
// ============================================================================

#[test]
fn jobs_event_codec_round_trips_submitted() {
    let event = JobsEvent::Submitted {
        job_id: "j1".into(),
        kind: "agent/duck".into(),
        submitter: actor("submitter"),
        spec: "summarize this".into(),
        spec_hash: vec![7u8; 32],
    };
    assert_eq!(
        decode_jobs_event(&encode_jobs_event(&event)).unwrap(),
        event
    );
    assert!(
        decode_jobs_event(b"not an event").is_err(),
        "bad event payloads must not decode"
    );
}

#[test]
fn register_worker_gating_idempotence_unregister_and_cap() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let empty_root = jobs.root();

        let err = stage(&mut jobs, 1, ext("operator"), register_worker())
            .await
            .expect_err("external registration rejected");
        assert!(matches!(err, Error::Module(m) if m.contains("module origin")));
        jobs.abort_block().await.unwrap();

        let err = stage(&mut jobs, 1, ext("operator"), unregister_worker())
            .await
            .expect_err("external unregistration rejected");
        assert!(matches!(err, Error::Module(m) if m.contains("module origin")));
        jobs.abort_block().await.unwrap();

        let err = stage(&mut jobs, 1, Origin::System, register_worker())
            .await
            .expect_err("system registration rejected");
        assert!(matches!(err, Error::Module(m) if m.contains("module origin")));
        jobs.abort_block().await.unwrap();

        let err = stage(&mut jobs, 1, Origin::System, unregister_worker())
            .await
            .expect_err("system unregistration rejected");
        assert!(matches!(err, Error::Module(m) if m.contains("module origin")));
        jobs.abort_block().await.unwrap();

        stage(
            &mut jobs,
            2,
            Origin::Module("agent".into()),
            register_worker(),
        )
        .await
        .expect("agent module self-registers");
        jobs.commit_block().await.unwrap();
        let registered_root = jobs.root();
        assert_ne!(registered_root, empty_root, "worker set is consensus state");

        stage(
            &mut jobs,
            3,
            Origin::Module("agent".into()),
            register_worker(),
        )
        .await
        .expect("re-register is idempotent");
        jobs.commit_block().await.unwrap();
        assert_eq!(
            jobs.root(),
            registered_root,
            "idempotent re-register moves no state"
        );

        stage(
            &mut jobs,
            4,
            Origin::Module("ghost".into()),
            unregister_worker(),
        )
        .await
        .expect("absent unregister is a deterministic no-op");
        jobs.commit_block().await.unwrap();
        assert_eq!(jobs.root(), registered_root);

        stage(
            &mut jobs,
            5,
            Origin::Module("agent".into()),
            unregister_worker(),
        )
        .await
        .expect("unregister existing worker");
        jobs.commit_block().await.unwrap();
        assert_eq!(
            jobs.root(),
            empty_root,
            "removing the only worker restores root"
        );

        for i in 0..MAX_WORKERS {
            let module = format!("worker-{i:02}");
            stage(
                &mut jobs,
                10 + i as u64,
                Origin::Module(module),
                register_worker(),
            )
            .await
            .expect("register within cap");
            jobs.commit_block().await.unwrap();
        }
        let err = stage(
            &mut jobs,
            99,
            Origin::Module("worker-overflow".into()),
            register_worker(),
        )
        .await
        .expect_err("worker cap enforced");
        assert!(matches!(err, Error::Module(m) if m.contains("worker cap reached")));
    });
}

// ============================================================================
// race resolution — first claim wins by consensus order
// ============================================================================

#[test]
fn second_claim_is_rejected_same_and_later_block() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("submitter"), submit("j1", "k", "")).await;

        // worker A wins the claim.
        apply(&mut jobs, 2, ext("worker-a"), claim("j1", 100)).await;

        // a second claim in a LATER block loses deterministically.
        let err = stage(&mut jobs, 3, ext("worker-b"), claim("j1", 100))
            .await
            .expect_err("second claim rejected");
        assert!(matches!(err, Error::Module(m) if m.contains("not claimable")));

        let job = get(&jobs, "j1").await.unwrap();
        assert_eq!(
            job.claim.unwrap().worker,
            actor("worker-a"),
            "A still owns it"
        );
        assert_eq!(job.attempt, 1, "the losing claim never bumped attempt");
    });
}

#[test]
fn wrong_worker_finalize_and_release_rejected() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("submitter"), submit("j1", "k", "")).await;
        apply(&mut jobs, 2, ext("worker-a"), claim("j1", 100)).await;

        let err = stage(&mut jobs, 3, ext("worker-b"), finalize("j1", true, "x"))
            .await
            .expect_err("wrong worker cannot finalize");
        assert!(
            matches!(err, Error::Module(m) if m.contains("only the current claimant may finalize"))
        );

        let err = stage(&mut jobs, 3, ext("worker-b"), release("j1"))
            .await
            .expect_err("wrong worker cannot release");
        assert!(
            matches!(err, Error::Module(m) if m.contains("only the current claimant may release"))
        );

        // the rightful claimant can both — release returns it to Pending, claim
        // cleared, attempt kept.
        apply(&mut jobs, 4, ext("worker-a"), release("j1")).await;
        let job = get(&jobs, "j1").await.unwrap();
        assert_eq!(job.status, JobStatus::Pending);
        assert!(job.claim.is_none());
        assert_eq!(job.attempt, 1, "release keeps the attempt count");
    });
}

#[test]
fn finalize_on_terminal_rejected_result_singularity() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("submitter"), submit("j1", "k", "")).await;
        apply(&mut jobs, 2, ext("worker-a"), claim("j1", 100)).await;
        apply(&mut jobs, 3, ext("worker-a"), finalize("j1", true, "first")).await;

        // a second finalize on the now-terminal job is rejected — the result is
        // singular.
        let err = stage(
            &mut jobs,
            4,
            ext("worker-a"),
            finalize("j1", false, "second"),
        )
        .await
        .expect_err("terminal job cannot be re-finalized");
        assert!(matches!(err, Error::Module(m) if m.contains("not in processing")));

        let job = get(&jobs, "j1").await.unwrap();
        assert_eq!(job.status, JobStatus::Done);
        assert_eq!(job.result.unwrap().payload, "first", "result unchanged");
    });
}

// ============================================================================
// permissionless reclaim on lease expiry
// ============================================================================

#[test]
fn premature_reclaim_rejected() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("submitter"), submit("j1", "k", "")).await;
        apply(&mut jobs, 5, ext("worker-a"), claim("j1", 10)).await; // deadline = 15

        // reclaim exactly AT the deadline is still premature (needs height > deadline).
        let err = stage(&mut jobs, 15, Origin::System, reclaim("j1"))
            .await
            .expect_err("reclaim at the deadline is premature");
        assert!(matches!(err, Error::Module(m) if m.contains("lease not expired")));

        // and well before it.
        let err = stage(&mut jobs, 10, ext("anyone"), reclaim("j1"))
            .await
            .expect_err("early reclaim rejected");
        assert!(matches!(err, Error::Module(m) if m.contains("lease not expired")));
    });
}

#[test]
fn expired_reclaim_requeues_attempt_kept_claim_cleared() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("submitter"), submit("j1", "k", "")).await;
        apply(&mut jobs, 5, ext("worker-a"), claim("j1", 10)).await; // deadline = 15

        // PERMISSIONLESS: a totally unrelated origin can crank the expiry.
        apply(&mut jobs, 16, ext("random-cranker"), reclaim("j1")).await;

        let job = get(&jobs, "j1").await.unwrap();
        assert_eq!(job.status, JobStatus::Pending);
        assert!(job.claim.is_none(), "claim cleared on requeue");
        assert_eq!(job.attempt, 1, "attempt count survives the requeue");
        assert_eq!(job.updated_at_height, 16);
    });
}

#[test]
fn attempts_exhausted_reclaim_fails_the_job() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("submitter"), submit("j1", "k", "")).await;

        // claim + expired-reclaim, over and over. each claim bumps attempt; each
        // expiry requeues — until attempt hits MAX_ATTEMPTS, when the reclaim
        // fails the job instead.
        let mut height = 10u64;
        loop {
            apply(&mut jobs, height, ext("worker-a"), claim("j1", 10)).await;
            height += 20; // safely past the deadline
            apply(&mut jobs, height, Origin::System, reclaim("j1")).await;
            height += 1;
            let job = get(&jobs, "j1").await.unwrap();
            if job.status != JobStatus::Pending {
                break;
            }
        }

        let job = get(&jobs, "j1").await.unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(job.attempt, MAX_ATTEMPTS);
        let r = job.result.unwrap();
        assert!(!r.ok);
        assert_eq!(r.payload, "attempts exhausted");
    });
}

// ============================================================================
// cancel / prune (any member; the submitter is attribution, not consent)
// ============================================================================

#[test]
fn cancel_only_from_pending_by_any_member() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("submitter"), submit("j1", "k", "")).await;

        // any member cancels a pending job, the submitter's attribution kept.
        apply(&mut jobs, 2, ext("intruder"), cancel("j1")).await;
        let cancelled = get(&jobs, "j1").await.unwrap();
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        assert_eq!(cancelled.submitter, actor("submitter"));

        // once claimed, even the submitter cannot cancel.
        apply(&mut jobs, 4, ext("submitter"), submit("j2", "k", "")).await;
        apply(&mut jobs, 5, ext("worker-a"), claim("j2", 100)).await;
        let err = stage(&mut jobs, 6, ext("submitter"), cancel("j2"))
            .await
            .expect_err("claimed job cannot be cancelled");
        assert!(matches!(err, Error::Module(m) if m.contains("cancel only applies to pending")));
    });
}

#[test]
fn prune_only_terminal_by_any_member_removes_record() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("submitter"), submit("j1", "k", "")).await;

        // a live (Pending) job cannot be pruned.
        let err = stage(&mut jobs, 2, ext("submitter"), prune("j1"))
            .await
            .expect_err("pending job cannot be pruned");
        assert!(matches!(err, Error::Module(m) if m.contains("prune only applies to terminal")));

        apply(&mut jobs, 3, ext("submitter"), cancel("j1")).await; // now terminal

        // any member prunes the record out of existence.
        let before = jobs.root();
        apply(&mut jobs, 4, ext("intruder"), prune("j1")).await;
        assert!(get(&jobs, "j1").await.is_none(), "record removed");
        assert_ne!(jobs.root(), before, "prune moves the committed root");
    });
}

// ============================================================================
// caps enforced at execute time (poison-value lesson)
// ============================================================================

#[test]
fn caps_rejection_table() {
    block_on(async {
        let mut jobs = jobs_on_mem();

        let too_long_id = "x".repeat(257);
        let too_long_kind = "k".repeat(MAX_KIND + 1);
        let over_spec = "s".repeat(MAX_SPEC + 1);
        let max_spec = "s".repeat(MAX_SPEC);

        let cases: Vec<(Msg, &str)> = vec![
            (submit("", "k", ""), "job_id must not be empty"),
            (submit(&too_long_id, "k", ""), "job_id exceeds"),
            (submit("ok", "", ""), "kind must not be empty"),
            (submit("ok", &too_long_kind, ""), "kind exceeds"),
            (submit("ok", "k", &over_spec), "spec exceeds"),
        ];
        for (msg, needle) in cases {
            let err = stage(&mut jobs, 1, ext("submitter"), msg)
                .await
                .expect_err("cap violation must reject");
            assert!(
                matches!(err, Error::Module(m) if m.contains(needle)),
                "expected `{needle}`"
            );
        }

        // spec exactly at the cap is accepted.
        apply(
            &mut jobs,
            1,
            ext("submitter"),
            submit("at-cap", "k", &max_spec),
        )
        .await;
        assert!(get(&jobs, "at-cap").await.is_some());

        // duplicate id is rejected.
        apply(&mut jobs, 2, ext("submitter"), submit("dup", "k", "")).await;
        let err = stage(&mut jobs, 3, ext("submitter"), submit("dup", "k", ""))
            .await
            .expect_err("duplicate rejected");
        assert!(matches!(err, Error::Module(m) if m.contains("already exists")));

        // payload cap on finalize.
        apply(&mut jobs, 4, ext("worker-a"), claim("dup", 100)).await;
        let over_payload = "p".repeat(MAX_PAYLOAD + 1);
        let err = stage(
            &mut jobs,
            5,
            ext("worker-a"),
            finalize("dup", true, &over_payload),
        )
        .await
        .expect_err("oversized payload rejected");
        assert!(matches!(err, Error::Module(m) if m.contains("payload exceeds")));
        // exactly at the cap is accepted.
        let max_payload = "p".repeat(MAX_PAYLOAD);
        apply(
            &mut jobs,
            6,
            ext("worker-a"),
            finalize("dup", true, &max_payload),
        )
        .await;
        assert_eq!(get(&jobs, "dup").await.unwrap().status, JobStatus::Done);
    });
}

#[test]
fn max_jobs_cap_is_overlay_aware() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        // fill the board to exactly MAX_JOBS distinct live ids, committing each
        // so the live-count stays O(1). the submitter rotates every
        // MAX_LIVE_JOBS_PER_SUBMITTER ids, so the GLOBAL cap is what trips
        // here and not the per-submitter one.
        for i in 0..MAX_JOBS {
            let submitter = ext(&format!("submitter-{}", i / MAX_LIVE_JOBS_PER_SUBMITTER));
            apply(
                &mut jobs,
                1,
                submitter,
                submit(&format!("job-{i:05}"), "k", ""),
            )
            .await;
        }
        // the next distinct id is refused -- from a fresh submitter, so only
        // the board-wide cap can be the reason.
        let err = stage(
            &mut jobs,
            1,
            ext("submitter-fresh"),
            submit("job-overflow", "k", ""),
        )
        .await
        .expect_err("board full");
        assert!(matches!(err, Error::Module(m) if m.contains("job board full")));
    });
}

/// ONE submitter cannot fill the shared board: it is refused BY NAME at
/// [`MAX_LIVE_JOBS_PER_SUBMITTER`] while another account still submits, and a
/// prune -- the only path that frees a slot -- lets it back in for exactly one.
#[test]
fn one_submitter_cannot_fill_the_board() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        for i in 0..MAX_LIVE_JOBS_PER_SUBMITTER {
            apply(
                &mut jobs,
                1,
                ext("greedy"),
                submit(&format!("greedy-{i:05}"), "k", ""),
            )
            .await;
        }

        let err = stage(&mut jobs, 1, ext("greedy"), submit("one-more", "k", ""))
            .await
            .expect_err("the submitter is at its cap");
        assert!(
            matches!(&err, Error::Module(m) if m.contains("job submitter at cap")),
            "the refusal names the cap: {err}"
        );
        assert!(get(&jobs, "one-more").await.is_none(), "nothing staged");

        // the board is nowhere near MAX_JOBS: a second account still submits.
        apply(&mut jobs, 2, ext("polite"), submit("polite-1", "k", "")).await;
        assert!(get(&jobs, "polite-1").await.is_some());

        // a terminal job still holds its slot -- only the prune frees one.
        apply(&mut jobs, 3, ext("greedy"), cancel("greedy-00000")).await;
        stage(&mut jobs, 3, ext("greedy"), submit("one-more", "k", ""))
            .await
            .expect_err("a cancelled record still occupies the board");
        jobs.abort_block().await.expect("abort");

        apply(&mut jobs, 4, ext("greedy"), prune("greedy-00000")).await;
        apply(&mut jobs, 5, ext("greedy"), submit("one-more", "k", "")).await;
        assert!(get(&jobs, "one-more").await.is_some());
        stage(&mut jobs, 6, ext("greedy"), submit("and-another", "k", ""))
            .await
            .expect_err("back at the cap after the one freed slot");
    });
}

#[test]
fn submitter_cap_is_shared_by_account_keys_and_isolated_per_program() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let account = key_account();
        let program = identity::AccountView {
            number: 2,
            name: "Program".into(),
            keys: vec![],
            control: identity::Control::Program {
                controller: account.number,
                executor: "runs".into(),
                generation: 0,
                standing: identity::ProgramStanding::Active,
            },
            ..account.clone()
        };
        // One uncommitted block exercises the staged counters. Alternating
        // keys cannot split account1's quota, and program2 owns its own quota.
        for i in 0..MAX_LIVE_JOBS_PER_SUBMITTER {
            let signer = ["founder", "sibling"][i % 2];
            jobs.execute(
                &mut account_ctx(1, ext(signer), &account),
                &submit(&format!("account-{i}"), "k", ""),
            )
            .await
            .unwrap();
            jobs.execute(
                &mut account_ctx(1, Origin::Program(2), &program),
                &submit(&format!("program-{i}"), "k", ""),
            )
            .await
            .unwrap();
        }
        jobs.commit_block().await.unwrap();
        let full_root = jobs.root();
        for (origin, identity) in [
            (ext("founder"), &account),
            (ext("sibling"), &account),
            (Origin::Program(2), &program),
        ] {
            let error = jobs
                .execute(
                    &mut account_ctx(2, origin, identity),
                    &submit("overflow", "k", ""),
                )
                .await
                .expect_err("the canonical submitter is at capacity");
            assert!(
                matches!(error, Error::Module(message) if message.contains("job submitter at cap"))
            );
        }
        jobs.commit_block().await.unwrap();
        assert_eq!(jobs.root(), full_root, "refusals stage no record or census");

        let other_program = identity::AccountView {
            number: 3,
            ..program.clone()
        };
        jobs.execute(
            &mut account_ctx(3, Origin::Program(3), &other_program),
            &submit("other-program", "k", ""),
        )
        .await
        .unwrap();
        for (id, origin) in [
            ("module", Origin::Module("runs".into())),
            ("system", Origin::System),
            ("key", ext("acct:2")),
        ] {
            stage(&mut jobs, 3, origin, submit(id, "k", ""))
                .await
                .unwrap();
        }
        jobs.commit_block().await.unwrap();
        assert_eq!(
            get(&jobs, "account-0").await.unwrap().submitter,
            tasks::Party::Account(1)
        );
        assert_eq!(
            get(&jobs, "program-0").await.unwrap().submitter,
            tasks::Party::Account(2)
        );
        assert_eq!(
            get(&jobs, "other-program").await.unwrap().submitter,
            tasks::Party::Account(3)
        );
        assert_eq!(
            get(&jobs, "module").await.unwrap().submitter,
            tasks::Party::Module("runs".into())
        );

        jobs.execute(
            &mut account_ctx(4, Origin::Program(2), &program),
            &cancel("program-0"),
        )
        .await
        .unwrap();
        // any member prunes: the controller account releases the program's
        // slot, and the census debits the PROGRAM, not the pruner.
        jobs.execute(
            &mut account_ctx(4, ext("founder"), &account),
            &prune("program-0"),
        )
        .await
        .unwrap();
        jobs.execute(
            &mut account_ctx(4, Origin::Program(2), &program),
            &submit("program-replacement", "k", ""),
        )
        .await
        .unwrap();
        jobs.commit_block().await.unwrap();
        jobs.execute(
            &mut account_ctx(5, Origin::Program(2), &program),
            &submit("program-overflow", "k", ""),
        )
        .await
        .expect_err("pruning releases exactly one program slot");
    });
}

#[test]
fn pruning_after_key_admission_debits_the_stored_key_quota() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let account = key_account();
        for i in 0..MAX_LIVE_JOBS_PER_SUBMITTER {
            stage(
                &mut jobs,
                1,
                ext("founder"),
                submit(&format!("key-{i}"), "k", ""),
            )
            .await
            .unwrap();
        }
        jobs.commit_block().await.unwrap();
        // Admission changes new ownership to Account, leaving old Key records
        // and their counters under the exact original signer.
        for i in 0..MAX_LIVE_JOBS_PER_SUBMITTER {
            jobs.execute(
                &mut account_ctx(2, ext("founder"), &account),
                &submit(&format!("account-{i}"), "k", ""),
            )
            .await
            .unwrap();
        }
        jobs.commit_block().await.unwrap();
        // an account sibling cancels and prunes the historical key's job; the
        // census still debits the exact original signer, never the sibling.
        jobs.execute(
            &mut account_ctx(3, ext("sibling"), &account),
            &cancel("key-0"),
        )
        .await
        .unwrap();
        jobs.commit_block().await.unwrap();
        jobs.execute(
            &mut account_ctx(4, ext("sibling"), &account),
            &prune("key-0"),
        )
        .await
        .unwrap();
        jobs.abort_block().await.unwrap();
        stage(
            &mut jobs,
            5,
            ext("founder"),
            submit("key-overflow", "k", ""),
        )
        .await
        .expect_err("aborting the prune restores the key census");
        jobs.execute(
            &mut account_ctx(5, ext("sibling"), &account),
            &prune("key-0"),
        )
        .await
        .unwrap();
        jobs.commit_block().await.unwrap();
        jobs.execute(
            &mut account_ctx(6, ext("sibling"), &account),
            &submit("account-overflow", "k", ""),
        )
        .await
        .expect_err("the old-key prune must not release an account slot");
        // Once that key leaves Identity, its exact remaining key-owned quota
        // is visible again: precisely the pruned slot is free.
        apply(
            &mut jobs,
            6,
            ext("founder"),
            submit("key-replacement", "k", ""),
        )
        .await;
        stage(
            &mut jobs,
            7,
            ext("founder"),
            submit("key-overflow", "k", ""),
        )
        .await
        .expect_err("only the stored key's one pruned slot was released");
    });
}

#[test]
fn applied_claim_and_index_agree_at_lease_bounds() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let mut rows = std::collections::BTreeMap::new();
        for (id, requested, expected) in [
            ("below", 0, tasks::MIN_LEASE_VIEWS),
            ("floor", tasks::MIN_LEASE_VIEWS, tasks::MIN_LEASE_VIEWS),
            ("ceiling", tasks::MAX_LEASE_VIEWS, tasks::MAX_LEASE_VIEWS),
            ("above", u64::MAX, tasks::MAX_LEASE_VIEWS),
        ] {
            for (height, msg) in [(1, submit(id, "k", "")), (2, claim(id, requested))] {
                let mut context = ctx(height, ext("worker-a"));
                jobs.execute(&mut context, &msg).await.unwrap();
                jobs.commit_block().await.unwrap();
                let op = index_guest::OpRow {
                    height,
                    seq: 0,
                    time: height,
                    origin: index_guest::OriginTag::external("raw-signer-label"),
                    payload: msg.payload,
                    assigned: context.assigned().unwrap().to_vec(),
                };
                let writes = tasks::index::fold_op(&op, &rows).unwrap();
                index_guest::apply_to_map(&mut rows, writes);
            }
            let committed = get(&jobs, id).await.unwrap();
            let claim = committed.claim.unwrap();
            assert_eq!(claim.lease_views, expected);
            let bytes = tasks::index::serve_view(&rows, br#"{"jobs":{}}"#).unwrap();
            let tasks::index::TasksViewReply::Jobs { jobs: indexed, .. } =
                serde_json::from_slice(&bytes).unwrap()
            else {
                panic!("expected indexed jobs");
            };
            let row = indexed.into_iter().find(|job| job.job_id == id).unwrap();
            assert_eq!(row.status, committed.status);
            assert_eq!(row.attempt, committed.attempt);
            let indexed_claim = row.claim.unwrap();
            assert_eq!(indexed_claim.lease_views, claim.lease_views, "{id}");
            assert_eq!(indexed_claim.claimed_at_height, claim.claimed_at_height);
            assert_eq!(indexed_claim.worker, ext("worker-a").actor_string());
        }
    });
}

// ============================================================================
// queries
// ============================================================================

#[test]
fn queries_get_hit_and_miss() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(
            &mut jobs,
            1,
            ext("submitter"),
            submit("alpha-1", "email", ""),
        )
        .await;
        apply(
            &mut jobs,
            1,
            ext("submitter"),
            submit("beta-1", "report", ""),
        )
        .await;
        apply(&mut jobs, 2, ext("worker-a"), claim("alpha-1", 100)).await;

        // Get is the whole kept dispatch surface: a hit answers the live
        // record per id, a miss answers None. board enumeration (status/kind
        // listings, the census) is index-tier — the index test
        // `job_lifecycle_moves_partitions_and_census`.
        assert_eq!(
            get(&jobs, "alpha-1").await.unwrap().status,
            JobStatus::Processing
        );
        assert_eq!(
            get(&jobs, "beta-1").await.unwrap().status,
            JobStatus::Pending
        );
        assert!(get(&jobs, "nope").await.is_none());
    });
}

// ============================================================================
// origin-derived identity
// ============================================================================

#[test]
fn identities_are_derived_from_origin() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(
            &mut jobs,
            1,
            Origin::Module("agent".into()),
            submit("j-mod", "k", ""),
        )
        .await;
        apply(&mut jobs, 1, ext("alice"), submit("j-ext", "k", "")).await;
        apply(&mut jobs, 1, Origin::System, submit("j-sys", "k", "")).await;

        assert_eq!(
            get(&jobs, "j-mod").await.unwrap().submitter,
            tasks::Party::Module("agent".into())
        );
        assert_eq!(get(&jobs, "j-ext").await.unwrap().submitter, actor("alice"));
        assert_eq!(
            get(&jobs, "j-sys").await.unwrap().submitter,
            tasks::Party::System
        );

        // the pre-consensus empty-external default is not an authenticated actor.
        let err = stage(
            &mut jobs,
            1,
            Origin::External(Vec::new()),
            submit("j-bad", "k", ""),
        )
        .await
        .expect_err("empty external origin rejected");
        assert!(matches!(err, Error::Module(m) if m.contains("non-empty submitter id")));
    });
}

// ============================================================================
// records / root
// ============================================================================

/// build a board exercising every status + both option fields.
async fn varied_board() -> Jobs {
    let mut jobs = jobs_on_mem();
    // pending
    apply(
        &mut jobs,
        1,
        ext("submitter"),
        submit("a-pending", "email", "s1"),
    )
    .await;
    // processing (claim retained)
    apply(
        &mut jobs,
        1,
        ext("submitter"),
        submit("b-processing", "report", "s2"),
    )
    .await;
    apply(&mut jobs, 2, ext("worker-a"), claim("b-processing", 42)).await;
    // done (result + claim)
    apply(
        &mut jobs,
        1,
        ext("submitter"),
        submit("c-done", "email", "s3"),
    )
    .await;
    apply(&mut jobs, 2, ext("worker-b"), claim("c-done", 100)).await;
    apply(
        &mut jobs,
        3,
        ext("worker-b"),
        finalize("c-done", true, "ok!"),
    )
    .await;
    // failed
    apply(
        &mut jobs,
        1,
        ext("submitter"),
        submit("d-failed", "email", "s4"),
    )
    .await;
    apply(&mut jobs, 2, ext("worker-b"), claim("d-failed", 100)).await;
    apply(
        &mut jobs,
        3,
        ext("worker-b"),
        finalize("d-failed", false, "nope"),
    )
    .await;
    // cancelled
    apply(
        &mut jobs,
        1,
        ext("submitter"),
        submit("e-cancelled", "report", "s5"),
    )
    .await;
    apply(&mut jobs, 4, ext("submitter"), cancel("e-cancelled")).await;
    jobs
}

/// every status shape survives as its own committed record, and `root()` is a
/// pure function of committed state (it reads the store's cached merkle root,
/// so repeated calls cannot drift). the cross-node round trip of these exact
/// records is `sync_round_trip`.
#[test]
fn every_status_shape_is_a_committed_record_and_root_is_stable() {
    block_on(async {
        let jobs = varied_board().await;
        let root = jobs.root();
        assert_eq!(jobs.root(), root, "root is stable across calls");
        assert_ne!(root, StateRoot::ZERO, "a populated board has a real root");

        for (id, status) in [
            ("a-pending", JobStatus::Pending),
            ("b-processing", JobStatus::Processing),
            ("c-done", JobStatus::Done),
            ("d-failed", JobStatus::Failed),
            ("e-cancelled", JobStatus::Cancelled),
        ] {
            assert_eq!(get(&jobs, id).await.expect("job exists").status, status);
        }

        // the module is qmdb-backed: sync rides the store's resolver lane.
        match jobs.state_sync_handle().expect("handle") {
            sdk::StateSyncHandle::ResolverBacked { backend, .. } => assert_eq!(backend, "qmdb"),
            other => panic!("expected ResolverBacked, got {other:?}"),
        }
    });
}

#[test]
fn claim_attempt_saturates_instead_of_wrapping() {
    block_on(async {
        // attempt counts are only ever produced by claim, so u64::MAX is not
        // execute-reachable: seed the store record directly (the store is
        // injected, so a test can write one) to prove the increment saturates
        // instead of wrapping.
        let mut store = MemStore::new();
        let job = serde_json::json!({
            "job_id": "j1",
            "conversation_id": "j1:1",
            "execution": "one_shot",
            "previous_job_id": null,
            "continuation_operation_id": null,
            "controls": [],
            "reports": [],
            "native_history": null,
            "kind": "k",
            "spec": "spec",
            "submitter": {"key": [0]},
            "status": "pending",
            "attempt": u64::MAX,
            "claim": null,
            "result": null,
            "comments": [],
            "created_at_revision": 1,
            "created_at_height": 1,
            "updated_at_height": 1,
        });
        store
            .commit_batch(vec![
                (
                    sdk::store_key(b"j/j1"),
                    Some(serde_json::to_vec(&job).unwrap()),
                ),
                (sdk::store_key(b"j#"), Some(1u64.to_le_bytes().to_vec())),
            ])
            .await
            .expect("seed the store");
        let mut jobs = Jobs::new(JOBS, "identity", "attribution", Box::new(store));

        apply(&mut jobs, 5, ext("worker-a"), claim("j1", 50)).await;
        let job = get(&jobs, "j1").await.expect("exists");
        assert_eq!(job.status, JobStatus::Processing);
        assert_eq!(job.attempt, u64::MAX, "saturated, not wrapped to zero");
    });
}

// ============================================================================
// committed-only query visibility
// ============================================================================

#[test]
fn queries_answer_committed_state_only() {
    block_on(async {
        let mut jobs = jobs_on_mem();

        // a staged-but-uncommitted submit is invisible to the read surface.
        stage(&mut jobs, 1, ext("submitter"), submit("j1", "k", ""))
            .await
            .expect("stage submit");
        assert!(get(&jobs, "j1").await.is_none(), "Get is blind to staging");

        // commit publishes; the same query now sees it.
        jobs.commit_block().await.expect("commit");
        assert_eq!(
            get(&jobs, "j1")
                .await
                .expect("committed, now visible")
                .status,
            JobStatus::Pending
        );

        // a staged transition is equally invisible: claim staged, Get still
        // reports the committed Pending.
        stage(&mut jobs, 2, ext("worker-a"), claim("j1", 50))
            .await
            .expect("stage claim");
        assert_eq!(
            get(&jobs, "j1").await.unwrap().status,
            JobStatus::Pending,
            "the staged claim is not served"
        );
        jobs.commit_block().await.expect("commit claim");
        assert_eq!(
            get(&jobs, "j1").await.unwrap().status,
            JobStatus::Processing
        );
    });
}

// ============================================================================
// staging semantics (commit publishes, abort discards)
// ============================================================================

#[test]
fn commit_and_abort_staging_including_prune_tombstones() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let root0 = jobs.root();

        // a staged submit moves neither the committed root nor the query view.
        stage(&mut jobs, 1, ext("submitter"), submit("j1", "k", ""))
            .await
            .expect("stage submit");
        assert_eq!(jobs.root(), root0, "staged write must not move the root");
        assert!(
            get(&jobs, "j1").await.is_none(),
            "queries answer committed state only"
        );

        jobs.commit_block().await.unwrap();
        let root1 = jobs.root();
        assert_ne!(root1, root0, "commit moves the root");
        assert!(get(&jobs, "j1").await.is_some(), "committed, now visible");

        // stage a prune tombstone: root unchanged and the committed record is
        // STILL served to queries (the tombstone lives only in the overlay).
        apply(&mut jobs, 2, ext("submitter"), cancel("j1")).await; // terminal
        let root2 = jobs.root();
        stage(&mut jobs, 3, ext("submitter"), prune("j1"))
            .await
            .expect("stage prune");
        assert_eq!(jobs.root(), root2, "staged prune must not move the root");
        assert!(
            get(&jobs, "j1").await.is_some(),
            "queries still serve the committed record"
        );

        // abort discards the tombstone, leaving everything byte-identical.
        jobs.abort_block().await.unwrap();
        assert_eq!(jobs.root(), root2, "abort keeps the root byte-identical");
        assert!(get(&jobs, "j1").await.is_some(), "the record survived");

        // committing the prune actually removes it and moves the root.
        apply(&mut jobs, 4, ext("submitter"), prune("j1")).await;
        assert!(get(&jobs, "j1").await.is_none());
        assert_ne!(jobs.root(), root2, "committed prune moves the root");
    });
}

// ============================================================================
// real-Host: first claim wins under the host's ordered dispatch
// ============================================================================

fn as_origin(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: 0,
        origin,
    }
}

async fn host_get(host: &Host, job_id: &str) -> Option<Job> {
    let bytes = host
        .query(
            JOBS,
            &encode_query(&JobsQuery::Get {
                job_id: job_id.into(),
            }),
        )
        .await
        .expect("host query");
    let JobsReply::Job(job) = decode_reply(&bytes).expect("decode") else {
        panic!("expected job reply")
    };
    job
}

/// a worker module that claims every submitted job it is notified about.
struct ClaimingWorker {
    id: ModuleId,
}

#[async_trait::async_trait(?Send)]
impl Module for ClaimingWorker {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let JobsEvent::Submitted { job_id, .. } =
            decode_jobs_event(&msg.payload).map_err(Error::Module)?;
        ctx.emit_msg(claim(&job_id, 100));
        Ok(())
    }
}

#[test]
fn host_submit_fans_out_to_registered_worker_and_claims_same_block() {
    block_on(async {
        let mut host = Host::genesis(vec![
            Box::new(identity::Identity::new(
                "identity",
                Box::new(MemStore::new()),
                "test".into(),
            )),
            Box::new(attribution::AttributionModule::new(
                "attribution",
                Box::new(MemStore::new()),
            )),
            Box::new(jobs_on_mem()),
            Box::new(ClaimingWorker { id: "agent".into() }),
        ])
        .expect("genesis");

        host.submit_at(
            as_origin(1, Origin::Module("agent".into())),
            register_worker(),
        )
        .await
        .expect("register worker");

        host.submit_at(
            as_origin(2, ext("submitter")),
            submit("j1", "agent/duck", "quack spec"),
        )
        .await
        .expect("submit cascades into claim");

        let job = host_get(&host, "j1").await.expect("job exists");
        assert_eq!(job.status, JobStatus::Processing);
        assert_eq!(
            job.claim.as_ref().map(|claim| &claim.worker),
            Some(&tasks::Party::Module("agent".into())),
            "the worker identity is host-assigned from the module origin"
        );
        assert_eq!(
            job.submitter,
            actor("submitter"),
            "the submitted event carried the origin-derived submitter"
        );
    });
}

#[test]
fn host_first_claim_wins_across_ordered_blocks() {
    block_on(async {
        let mut host = Host::genesis(vec![
            Box::new(identity::Identity::new(
                "identity",
                Box::new(MemStore::new()),
                "test".into(),
            )),
            Box::new(attribution::AttributionModule::new(
                "attribution",
                Box::new(MemStore::new()),
            )),
            Box::new(jobs_on_mem()),
        ])
        .expect("genesis");

        host.submit_at(as_origin(1, ext("submitter")), submit("j1", "k", ""))
            .await
            .expect("submit");

        // block 2: worker A claims — commits.
        host.submit_at(as_origin(2, ext("worker-a")), claim("j1", 100))
            .await
            .expect("A wins the claim");
        let after_a = host.module_root(JOBS).expect("root");

        // block 3: worker B claims the same job — rejected, block aborts.
        let err = host
            .submit_at(as_origin(3, ext("worker-b")), claim("j1", 100))
            .await
            .expect_err("B loses the race");
        assert!(matches!(
            err,
            SubmitError::Rejected(Error::Module(ref m)) if m.contains("not claimable")
        ));

        // the losing claim left no trace: root unchanged, A still the claimant.
        assert_eq!(
            host.module_root(JOBS).unwrap(),
            after_a,
            "loser did not mutate state"
        );
        let job = host_get(&host, "j1").await.unwrap();
        assert_eq!(job.claim.unwrap().worker, actor("worker-a"));
    });
}

/// a helper module that, in ONE block, emits two claims on the same job.
struct DoubleClaim {
    job_id: String,
}

#[async_trait::async_trait(?Send)]
impl Module for DoubleClaim {
    fn id(&self) -> ModuleId {
        "double-claim".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        ctx.emit_msg(claim(&self.job_id, 100));
        ctx.emit_msg(claim(&self.job_id, 100));
        Ok(())
    }
}

#[test]
fn host_two_claims_in_one_block_abort_atomically() {
    block_on(async {
        let mut host = Host::genesis(vec![
            Box::new(identity::Identity::new(
                "identity",
                Box::new(MemStore::new()),
                "test".into(),
            )),
            Box::new(attribution::AttributionModule::new(
                "attribution",
                Box::new(MemStore::new()),
            )),
            Box::new(jobs_on_mem()),
            Box::new(DoubleClaim {
                job_id: "j1".into(),
            }),
        ])
        .expect("genesis");

        host.submit_at(as_origin(1, ext("submitter")), submit("j1", "k", ""))
            .await
            .expect("submit");
        let before = host.module_root(JOBS).expect("root");

        // one block, two claims: the second sees the first's staged Processing
        // and rejects, so the WHOLE block aborts (atomicity).
        let err = host
            .submit_at(
                as_origin(2, ext("trigger")),
                Msg {
                    target: "double-claim".into(),
                    payload: Vec::new(),
                },
            )
            .await
            .expect_err("the second claim aborts the block");
        assert!(matches!(
            err,
            SubmitError::Rejected(Error::Module(ref m)) if m.contains("not claimable")
        ));

        // nothing committed: the job is still exactly Pending, root byte-identical.
        assert_eq!(host.module_root(JOBS).unwrap(), before);
        assert_eq!(
            host_get(&host, "j1").await.unwrap().status,
            JobStatus::Pending
        );
    });
}

#[test]
fn job_comments_preserve_authorship_status_and_bounded_immutable_history() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit("discussion", "build", "spec"),
        )
        .await;
        apply(&mut jobs, 2, ext("worker"), claim("discussion", 100)).await;
        for i in 0..tasks::MAX_JOB_COMMENTS {
            apply(
                &mut jobs,
                3 + i as u64,
                ext("alice"),
                jobs_msg(JobsMsg::Comment {
                    created_at_revision: 1,
                    job_id: "discussion".into(),
                    comment_id: format!("c{i}"),
                    text: format!("Update {i}"),
                }),
            )
            .await;
        }
        let job = get(&jobs, "discussion").await.unwrap();
        assert_eq!(job.status, JobStatus::Processing);
        assert_eq!(job.claim.as_ref().unwrap().worker, actor("worker"));
        assert_eq!(job.comments.len(), tasks::MAX_JOB_COMMENTS);
        assert_eq!(job.comments[0].author, actor("alice"));
        assert_eq!(job.comments[0].height, 3);
        assert_eq!(job.comments[0].text, "Update 0");
        let root = jobs.root();
        for (id, text) in [
            ("overflow", "too late".to_string()),
            ("c0", "overwrite".into()),
            ("blank", " ".into()),
            ("large", "x".repeat(tasks::MAX_JOB_COMMENT_TEXT_BYTES + 1)),
        ] {
            assert!(
                stage(
                    &mut jobs,
                    100,
                    ext("worker"),
                    jobs_msg(JobsMsg::Comment {
                        created_at_revision: 1,
                        job_id: "discussion".into(),
                        comment_id: id.into(),
                        text
                    })
                )
                .await
                .is_err()
            );
            jobs.commit_block().await.unwrap();
            assert_eq!(jobs.root(), root);
        }
        apply(
            &mut jobs,
            101,
            ext("worker"),
            finalize("discussion", true, "done"),
        )
        .await;
        assert_eq!(
            get(&jobs, "discussion").await.unwrap().comments,
            job.comments
        );
    });
}

#[test]
fn a_job_comment_cannot_be_overwritten_by_another_actor() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("alice"), submit("j", "build", "spec")).await;
        apply(
            &mut jobs,
            2,
            ext("alice"),
            jobs_msg(JobsMsg::Comment {
                created_at_revision: 1,
                job_id: "j".into(),
                comment_id: "c".into(),
                text: "Original".into(),
            }),
        )
        .await;
        let before = jobs.root();
        let error = stage(
            &mut jobs,
            3,
            ext("bob"),
            jobs_msg(JobsMsg::Comment {
                created_at_revision: 1,
                job_id: "j".into(),
                comment_id: "c".into(),
                text: "Forged".into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(format!("{error}").contains("already exists"));
        jobs.commit_block().await.unwrap();
        assert_eq!(jobs.root(), before);
        let job = get(&jobs, "j").await.unwrap();
        assert_eq!(job.comments[0].author, actor("alice"));
        assert_eq!(job.comments[0].text, "Original");
    });
}

async fn history(jobs: &Jobs, conversation_id: &str) -> tasks::WorkerHistory {
    let bytes = jobs
        .query(&encode_query(&JobsQuery::GetWorker {
            conversation_id: conversation_id.into(),
        }))
        .await
        .unwrap();
    let JobsReply::Worker(Some(history)) = decode_reply(&bytes).unwrap() else {
        panic!("worker history");
    };
    history
}

fn submit_conversation(job_id: &str, kind: &str, spec: &str) -> Msg {
    jobs_msg(JobsMsg::SubmitConversation {
        job_id: job_id.into(),
        kind: kind.into(),
        spec: spec.into(),
    })
}

fn control(job_id: &str, operation_id: &str, input: tasks::JobControlInput) -> Msg {
    jobs_msg(JobsMsg::Control {
        job_id: job_id.into(),
        operation_id: operation_id.into(),
        input,
    })
}

fn acknowledge(job_id: &str, operation_id: &str, attempt: u64) -> Msg {
    jobs_msg(JobsMsg::AcknowledgeControl {
        job_id: job_id.into(),
        operation_id: operation_id.into(),
        attempt,
    })
}

fn checkpoint(job_id: &str, operation_id: &str, attempt: u64) -> Msg {
    jobs_msg(JobsMsg::Checkpoint {
        job_id: job_id.into(),
        operation_id: operation_id.into(),
        attempt,
        kind: tasks::WorkerReportKind::Checkpoint,
        payload: "Durable evidence".into(),
    })
}

fn native_checkpoint(
    job_id: &str,
    attempt: u64,
    run_id: &str,
    execution_attempt: u32,
    revision: u64,
    snapshot: &str,
) -> Msg {
    jobs_msg(JobsMsg::CheckpointNativeHistory {
        job_id: job_id.into(),
        attempt,
        run_id: run_id.into(),
        execution_attempt,
        revision,
        snapshot: snapshot.into(),
    })
}

fn settle(job_id: &str, operation_id: &str, attempt: u64) -> Msg {
    jobs_msg(JobsMsg::SettleCancellation {
        job_id: job_id.into(),
        operation_id: operation_id.into(),
        attempt,
        payload: "Stopped at safe boundary".into(),
    })
}

fn continuation(previous_job_id: &str, job_id: &str, operation_id: &str, kind: &str) -> Msg {
    jobs_msg(JobsMsg::Continue {
        previous_job_id: previous_job_id.into(),
        job_id: job_id.into(),
        operation_id: operation_id.into(),
        kind: kind.into(),
        spec: "Continue from retained evidence".into(),
    })
}

async fn rejected(jobs: &mut Jobs, origin: Origin, msg: Msg) {
    let root = jobs.root();
    stage(jobs, 20, origin, msg).await.expect_err("must reject");
    jobs.commit_block().await.unwrap();
    assert_eq!(jobs.root(), root, "rejection stages nothing");
}

#[test]
fn steering_is_explicit_and_acknowledged_only_by_current_claim_attempt() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let worker = Origin::Module("runs".into());
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit_conversation("j", "agent", "Work"),
        )
        .await;
        apply(&mut jobs, 2, worker.clone(), claim("j", 100)).await;
        let original = get(&jobs, "j").await.unwrap();
        apply(
            &mut jobs,
            3,
            ext("alice"),
            jobs_msg(JobsMsg::Comment {
                job_id: "j".into(),
                created_at_revision: original.created_at_revision,
                comment_id: "discussion".into(),
                text: "Please change direction".into(),
            }),
        )
        .await;
        assert!(get(&jobs, "j").await.unwrap().controls.is_empty());
        let steer = control(
            "j",
            "steer",
            tasks::JobControlInput::Steer {
                text: "Change direction".into(),
            },
        );
        apply(&mut jobs, 4, ext("bob"), steer.clone()).await;
        let root = jobs.root();
        apply(&mut jobs, 5, ext("bob"), steer).await;
        assert_eq!(jobs.root(), root, "identical control is idempotent");
        rejected(&mut jobs, ext("eve"), acknowledge("j", "steer", 1)).await;
        rejected(&mut jobs, worker.clone(), acknowledge("j", "missing", 1)).await;
        rejected(&mut jobs, worker.clone(), acknowledge("j", "steer", 2)).await;
        apply(&mut jobs, 6, worker.clone(), acknowledge("j", "steer", 1)).await;
        let root = jobs.root();
        apply(&mut jobs, 7, worker.clone(), acknowledge("j", "steer", 1)).await;
        assert_eq!(jobs.root(), root, "receipt is idempotent");
        apply(&mut jobs, 8, worker.clone(), release("j")).await;
        apply(&mut jobs, 9, worker.clone(), claim("j", 100)).await;
        rejected(&mut jobs, worker.clone(), acknowledge("j", "steer", 1)).await;
        rejected(&mut jobs, worker.clone(), checkpoint("j", "old-attempt", 1)).await;
        apply(&mut jobs, 10, worker.clone(), acknowledge("j", "steer", 2)).await;
        apply(&mut jobs, 11, worker, checkpoint("j", "current", 2)).await;
        let job = get(&jobs, "j").await.unwrap();
        assert_eq!(job.conversation_id, original.conversation_id);
        assert_eq!(job.status, JobStatus::Processing);
        assert_eq!(job.controls[0].author, actor("bob"));
        assert_eq!(
            job.controls[0]
                .acknowledgements
                .iter()
                .map(|ack| ack.attempt)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(job.reports[0].worker, tasks::Party::Module("runs".into()));
        assert_eq!(job.reports[0].attempt, 2);
        assert_eq!(job.reports[0].height, 11);
    });
}

#[test]
fn cancellation_request_acknowledgement_and_settlement_are_distinct() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let worker = Origin::Module("runs".into());
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit_conversation("j", "agent", "Work"),
        )
        .await;
        apply(
            &mut jobs,
            2,
            ext("bob"),
            control("j", "stop", tasks::JobControlInput::Cancel),
        )
        .await;
        assert_eq!(get(&jobs, "j").await.unwrap().status, JobStatus::Pending);
        rejected(&mut jobs, worker.clone(), settle("j", "stop", 0)).await;
        apply(&mut jobs, 3, worker.clone(), claim("j", 100)).await;
        rejected(&mut jobs, worker.clone(), settle("j", "stop", 1)).await;
        apply(&mut jobs, 4, worker.clone(), acknowledge("j", "stop", 1)).await;
        let acknowledged = get(&jobs, "j").await.unwrap();
        assert_eq!(acknowledged.status, JobStatus::Processing);
        assert!(acknowledged.result.is_none());
        apply(&mut jobs, 5, worker.clone(), release("j")).await;
        apply(&mut jobs, 6, worker.clone(), claim("j", 100)).await;
        rejected(&mut jobs, worker.clone(), settle("j", "stop", 1)).await;
        rejected(&mut jobs, worker.clone(), settle("j", "stop", 2)).await;
        apply(&mut jobs, 7, worker.clone(), acknowledge("j", "stop", 2)).await;
        rejected(&mut jobs, ext("eve"), settle("j", "stop", 2)).await;
        apply(&mut jobs, 8, worker.clone(), settle("j", "stop", 2)).await;
        let settled = get(&jobs, "j").await.unwrap();
        assert_eq!(settled.status, JobStatus::Cancelled);
        assert_eq!(settled.result.unwrap().payload, "Stopped at safe boundary");
        assert_eq!(settled.reports[0].operation_id, "stop");
        assert_eq!(settled.reports[0].attempt, 2);
        assert_eq!(
            settled.reports[0].worker,
            tasks::Party::Module("runs".into())
        );
        rejected(&mut jobs, worker.clone(), finalize("j", true, "late")).await;
        rejected(&mut jobs, worker.clone(), checkpoint("j", "late", 2)).await;
        rejected(&mut jobs, worker, settle("j", "stop", 2)).await;
        rejected(
            &mut jobs,
            ext("alice"),
            control("j", "late", tasks::JobControlInput::Cancel),
        )
        .await;
    });
}

#[test]
fn prune_retains_conversation_and_continue_creates_one_new_execution() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit_conversation("original", "agent", "Work"),
        )
        .await;
        rejected(
            &mut jobs,
            ext("bob"),
            continuation("original", "next", "continue", "agent"),
        )
        .await;
        apply(&mut jobs, 2, ext("worker"), claim("original", 100)).await;
        apply(
            &mut jobs,
            3,
            ext("worker"),
            checkpoint("original", "evidence", 1),
        )
        .await;
        apply(
            &mut jobs,
            3,
            ext("worker"),
            native_checkpoint("original", 1, "run-original", 0, 1, "snapshot-original"),
        )
        .await;
        apply(
            &mut jobs,
            4,
            ext("worker"),
            finalize("original", true, "Complete"),
        )
        .await;
        let original = get(&jobs, "original").await.unwrap();
        apply(&mut jobs, 5, ext("bob"), prune("original")).await;
        assert!(get(&jobs, "original").await.is_none());
        assert_eq!(
            history(&jobs, &original.conversation_id).await.executions,
            vec![original.clone()]
        );
        apply(
            &mut jobs,
            6,
            Origin::Module("runs".into()),
            register_worker(),
        )
        .await;
        let next = continuation("original", "next", "continue", "agent");
        let mut dispatch = ctx(7, ext("bob"));
        jobs.execute(&mut dispatch, &next).await.unwrap();
        let notifications = dispatch
            .msgs()
            .iter()
            .filter(|msg| msg.target == "runs")
            .collect::<Vec<_>>();
        assert_eq!(notifications.len(), 1);
        let JobsEvent::Submitted {
            job_id,
            submitter,
            spec,
            ..
        } = decode_jobs_event(&notifications[0].payload).unwrap();
        assert_eq!(job_id, "next");
        assert_eq!(submitter, original.submitter);
        assert_eq!(spec, "Continue from retained evidence");
        assert!(
            get(&jobs, "next").await.is_none(),
            "submit event does not expose uncommitted jobs"
        );
        jobs.commit_block().await.unwrap();
        let execution = get(&jobs, "next").await.unwrap();
        assert_eq!(execution.conversation_id, original.conversation_id);
        assert_eq!(execution.execution, tasks::JobExecution::Conversation);
        assert_eq!(execution.previous_job_id.as_deref(), Some("original"));
        assert_eq!(
            execution.continuation_operation_id.as_deref(),
            Some("continue")
        );
        assert_eq!(execution.status, JobStatus::Pending);
        assert_eq!(execution.attempt, 0);
        assert!(execution.claim.is_none());
        assert!(execution.controls.is_empty());
        assert!(execution.reports.is_empty());
        assert!(execution.native_history.is_none());
        let root = jobs.root();
        let mut retry = ctx(8, ext("bob"));
        jobs.execute(&mut retry, &next).await.unwrap();
        assert!(
            retry.msgs().is_empty(),
            "retry emits neither submit nor attribution"
        );
        jobs.commit_block().await.unwrap();
        assert_eq!(jobs.root(), root);
        rejected(
            &mut jobs,
            ext("bob"),
            continuation("original", "fork", "other", "agent"),
        )
        .await;
        assert_eq!(
            history(&jobs, &original.conversation_id).await.executions,
            vec![original.clone(), execution]
        );
        apply(
            &mut jobs,
            9,
            ext("alice"),
            submit("original", "different", "Fresh work"),
        )
        .await;
        let reused = get(&jobs, "original").await.unwrap();
        assert_ne!(reused.conversation_id, original.conversation_id);
        assert_eq!(
            history(&jobs, &original.conversation_id).await.executions[0],
            original
        );
    });
}

#[test]
fn continuation_selects_a_different_worker_kind_without_rewriting_predecessor_or_history() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let worker = Origin::Module("runs".into());
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit_conversation("research", "researcher", "Investigate"),
        )
        .await;
        apply(&mut jobs, 2, worker.clone(), claim("research", 100)).await;
        apply(
            &mut jobs,
            3,
            worker.clone(),
            native_checkpoint("research", 1, "run-research", 0, 17, "snapshot-research"),
        )
        .await;
        apply(
            &mut jobs,
            4,
            worker.clone(),
            checkpoint("research", "evidence", 1),
        )
        .await;
        apply(
            &mut jobs,
            5,
            worker.clone(),
            finalize("research", true, "Evidence ready"),
        )
        .await;
        let predecessor = get(&jobs, "research").await.unwrap();
        for kind in [String::new(), "x".repeat(MAX_KIND + 1)] {
            rejected(
                &mut jobs,
                ext("bob"),
                continuation("research", "write", "handoff", &kind),
            )
            .await;
        }
        apply(&mut jobs, 6, worker, register_worker()).await;
        let next = continuation("research", "write", "handoff", "writer");
        let mut dispatch = ctx(7, ext("bob"));
        jobs.execute(&mut dispatch, &next).await.unwrap();
        let notification = dispatch
            .msgs()
            .iter()
            .find(|message| message.target == "runs")
            .unwrap();
        let JobsEvent::Submitted {
            job_id,
            kind,
            submitter,
            ..
        } = decode_jobs_event(&notification.payload).unwrap();
        assert_eq!(job_id, "write");
        assert_eq!(kind, "writer");
        assert_eq!(submitter, predecessor.submitter);
        jobs.commit_block().await.unwrap();
        let continued = get(&jobs, "write").await.unwrap();
        assert_eq!(continued.kind, "writer");
        assert_eq!(continued.execution, tasks::JobExecution::Conversation);
        assert_eq!(continued.conversation_id, predecessor.conversation_id);
        assert_eq!(continued.submitter, predecessor.submitter);
        assert_eq!(continued.previous_job_id.as_deref(), Some("research"));
        assert_eq!(
            continued.continuation_operation_id.as_deref(),
            Some("handoff")
        );
        assert_eq!(continued.attempt, 0);
        assert!(continued.native_history.is_none());
        assert_eq!(get(&jobs, "research").await, Some(predecessor.clone()));
        assert_eq!(
            history(&jobs, &predecessor.conversation_id)
                .await
                .executions,
            vec![predecessor.clone(), continued.clone()]
        );
        let root = jobs.root();
        let mut retry = ctx(8, ext("bob"));
        jobs.execute(&mut retry, &next).await.unwrap();
        assert!(
            retry.msgs().is_empty(),
            "same kind is an exact idempotent retry"
        );
        jobs.commit_block().await.unwrap();
        assert_eq!(jobs.root(), root);
        rejected(
            &mut jobs,
            ext("bob"),
            continuation("research", "write", "handoff", "reviewer"),
        )
        .await;
        assert_eq!(get(&jobs, "research").await, Some(predecessor.clone()));
        apply(&mut jobs, 9, ext("bob"), prune("research")).await;
        assert_eq!(
            history(&jobs, &predecessor.conversation_id)
                .await
                .executions,
            vec![predecessor, continued]
        );
    });
}

#[test]
fn retained_inputs_are_committed_only_and_abort_discards_archive_changes() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit_conversation("j", "agent", "Work"),
        )
        .await;
        apply(&mut jobs, 2, ext("worker"), claim("j", 100)).await;
        let original = get(&jobs, "j").await.unwrap();
        let root = jobs.root();
        stage(
            &mut jobs,
            3,
            ext("alice"),
            control(
                "j",
                "steer",
                tasks::JobControlInput::Steer {
                    text: "Change".into(),
                },
            ),
        )
        .await
        .unwrap();
        stage(&mut jobs, 3, ext("worker"), acknowledge("j", "steer", 1))
            .await
            .unwrap();
        stage(&mut jobs, 3, ext("worker"), checkpoint("j", "evidence", 1))
            .await
            .unwrap();
        stage(
            &mut jobs,
            3,
            ext("worker"),
            native_checkpoint("j", 1, "run", 0, 1, "snapshot-1"),
        )
        .await
        .unwrap();
        let query = encode_query(&JobsQuery::Controls { job_id: "j".into() });
        assert_eq!(
            decode_reply(&jobs.query(&query).await.unwrap()).unwrap(),
            JobsReply::Controls(Vec::new())
        );
        assert_eq!(
            history(&jobs, &original.conversation_id).await.executions,
            vec![original.clone()]
        );
        jobs.abort_block().await.unwrap();
        jobs.commit_block().await.unwrap();
        assert_eq!(jobs.root(), root);
        assert_eq!(
            history(&jobs, &original.conversation_id).await.executions,
            vec![original]
        );
    });
}

#[test]
fn control_and_checkpoint_caps_reject_conflicts_but_reserve_cancellation_report() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit_conversation("j", "agent", "Work"),
        )
        .await;
        apply(&mut jobs, 2, ext("worker"), claim("j", 100)).await;
        rejected(
            &mut jobs,
            ext("alice"),
            control(
                "j",
                "bad",
                tasks::JobControlInput::Steer { text: " ".into() },
            ),
        )
        .await;
        rejected(
            &mut jobs,
            ext("alice"),
            control(
                "j",
                "bad",
                tasks::JobControlInput::Steer {
                    text: "x".repeat(tasks::MAX_WORKER_TEXT_BYTES + 1),
                },
            ),
        )
        .await;
        apply(
            &mut jobs,
            3,
            ext("alice"),
            control("j", "stop", tasks::JobControlInput::Cancel),
        )
        .await;
        rejected(&mut jobs, ext("worker"), checkpoint("j", "stop", 1)).await;
        for i in 1..tasks::MAX_JOB_CONTROLS {
            apply(
                &mut jobs,
                3,
                ext("alice"),
                control("j", &format!("control-{i}"), tasks::JobControlInput::Cancel),
            )
            .await;
        }
        rejected(
            &mut jobs,
            ext("alice"),
            control("j", "overflow", tasks::JobControlInput::Cancel),
        )
        .await;
        rejected(
            &mut jobs,
            ext("bob"),
            control("j", "stop", tasks::JobControlInput::Cancel),
        )
        .await;
        for i in 0..tasks::MAX_WORKER_REPORTS {
            apply(
                &mut jobs,
                4,
                ext("worker"),
                checkpoint("j", &format!("report-{i}"), 1),
            )
            .await;
        }
        let root = jobs.root();
        apply(&mut jobs, 5, ext("worker"), checkpoint("j", "report-0", 1)).await;
        assert_eq!(jobs.root(), root);
        rejected(&mut jobs, ext("worker"), checkpoint("j", "overflow", 1)).await;
        apply(&mut jobs, 6, ext("worker"), acknowledge("j", "stop", 1)).await;
        apply(&mut jobs, 7, ext("worker"), settle("j", "stop", 1)).await;
        let job = get(&jobs, "j").await.unwrap();
        assert_eq!(job.reports.len(), tasks::MAX_WORKER_REPORTS + 1);
        apply(&mut jobs, 8, ext("alice"), prune("j")).await;
        let query = encode_query(&JobsQuery::Controls { job_id: "j".into() });
        assert_eq!(
            decode_reply(&jobs.query(&query).await.unwrap()).unwrap(),
            JobsReply::Controls(job.controls.clone())
        );
        assert_eq!(
            history(&jobs, &job.conversation_id).await.executions,
            vec![job]
        );
    });
}

#[test]
fn native_history_replaces_beyond_report_cap_without_notifications_or_source_revisions() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let worker = Origin::Module("runs".into());
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit_conversation("j", "worker", "Work"),
        )
        .await;
        apply(&mut jobs, 2, worker.clone(), claim("j", 100)).await;
        let before = get(&jobs, "j").await.unwrap();
        let root = jobs.root();
        for revision in 1..=70 {
            let mut dispatch = ctx(3 + revision, worker.clone());
            jobs.execute(
                &mut dispatch,
                &native_checkpoint("j", 1, "run", 0, revision, &format!("snapshot-{revision}")),
            )
            .await
            .unwrap();
            assert!(
                dispatch.msgs().is_empty(),
                "machine snapshots emit no attribution or worker notification"
            );
            jobs.commit_block().await.unwrap();
        }
        assert_ne!(jobs.root(), root, "native head is consensus state");
        let current = get(&jobs, "j").await.unwrap();
        assert!(current.reports.is_empty());
        assert_eq!(
            current.native_history,
            Some(tasks::NativeHistoryHead {
                job_attempt: 1,
                worker: tasks::Party::Module("runs".into()),
                run_id: "run".into(),
                execution_attempt: 0,
                revision: 70,
                snapshot: "snapshot-70".into(),
                height: 73,
            })
        );
        assert_eq!(
            history(&jobs, &before.conversation_id).await.executions,
            vec![current.clone()]
        );
        let root = jobs.root();
        apply(
            &mut jobs,
            74,
            worker.clone(),
            native_checkpoint("j", 1, "run", 0, 70, "snapshot-70"),
        )
        .await;
        assert_eq!(
            jobs.root(),
            root,
            "exact duplicate does not replace provenance"
        );
        let mut progress = ctx(75, worker.clone());
        jobs.execute(&mut progress, &checkpoint("j", "semantic-0", 1))
            .await
            .unwrap();
        let published = progress
            .msgs()
            .iter()
            .find(|message| message.target == "attribution")
            .unwrap();
        let attribution::AttributionMsg::Attribute { revision, .. } =
            attribution::decode_msg(&published.payload).unwrap()
        else {
            panic!("attribution");
        };
        assert_eq!(
            revision, 3,
            "machine heads do not consume ordinary source revisions"
        );
        jobs.commit_block().await.unwrap();
        for i in 1..tasks::MAX_WORKER_REPORTS {
            apply(
                &mut jobs,
                76,
                worker.clone(),
                checkpoint("j", &format!("semantic-{i}"), 1),
            )
            .await;
        }
        rejected(&mut jobs, worker.clone(), checkpoint("j", "overflow", 1)).await;
        apply(
            &mut jobs,
            77,
            worker,
            native_checkpoint("j", 1, "run", 0, 71, "snapshot-71"),
        )
        .await;
        assert_eq!(
            get(&jobs, "j").await.unwrap().reports.len(),
            tasks::MAX_WORKER_REPORTS
        );
        assert_eq!(
            get(&jobs, "j")
                .await
                .unwrap()
                .native_history
                .unwrap()
                .revision,
            71
        );
    });
}

#[test]
fn native_history_accepts_opaque_runs_keys_with_internal_separators_within_byte_cap() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let worker = Origin::Module("runs".into());
        let job_id = "j".repeat(tasks::MAX_JOB_ID);
        let agent_id = "a".repeat(256);
        let run_id = format!("job\u{1f}{job_id}\u{1f}{agent_id}\u{1f}2");
        assert!(
            run_id.len() > tasks::MAX_JOB_ID,
            "a composed Runs key is longer than a public job ID"
        );
        let maximum = format!(
            "job\u{1f}{}",
            "x".repeat(tasks::MAX_NATIVE_RUN_ID_BYTES - 4)
        );
        for (job_id, run_id) in [(job_id.as_str(), run_id), ("maximum", maximum)] {
            apply(
                &mut jobs,
                1,
                ext("alice"),
                submit_conversation(job_id, "worker", "Work"),
            )
            .await;
            apply(&mut jobs, 2, worker.clone(), claim(job_id, 100)).await;
            rejected(
                &mut jobs,
                worker.clone(),
                native_checkpoint(job_id, 1, "", 0, 1, "snapshot"),
            )
            .await;
            rejected(
                &mut jobs,
                worker.clone(),
                native_checkpoint(
                    job_id,
                    1,
                    &"x".repeat(tasks::MAX_NATIVE_RUN_ID_BYTES + 1),
                    0,
                    1,
                    "snapshot",
                ),
            )
            .await;
            let snapshot = native_checkpoint(job_id, 1, &run_id, 0, 1, "snapshot");
            rejected(&mut jobs, ext("intruder"), snapshot.clone()).await;
            rejected(
                &mut jobs,
                worker.clone(),
                native_checkpoint(job_id, 2, &run_id, 0, 1, "snapshot"),
            )
            .await;
            apply(&mut jobs, 3, worker.clone(), snapshot.clone()).await;
            let head = get(&jobs, job_id).await.unwrap().native_history.unwrap();
            assert_eq!(head.run_id, run_id);
            assert_eq!(head.job_attempt, 1);
            assert_eq!(head.worker, tasks::Party::Module("runs".into()));
            let root = jobs.root();
            apply(&mut jobs, 4, worker.clone(), snapshot).await;
            assert_eq!(
                jobs.root(),
                root,
                "opaque-key exact replay is still idempotent"
            );
        }
    });
}

#[test]
fn native_history_fences_claim_run_execution_and_revision_and_survives_prune() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        let worker = Origin::Module("runs".into());
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit_conversation("j", "worker", "Work"),
        )
        .await;
        apply(&mut jobs, 2, worker.clone(), claim("j", 100)).await;
        rejected(
            &mut jobs,
            worker.clone(),
            native_checkpoint("j", 1, "run", 0, 0, "snapshot"),
        )
        .await;
        rejected(
            &mut jobs,
            worker.clone(),
            native_checkpoint("j", 1, "", 0, 1, "snapshot"),
        )
        .await;
        rejected(
            &mut jobs,
            worker.clone(),
            native_checkpoint("j", 1, "run", 0, 1, ""),
        )
        .await;
        rejected(
            &mut jobs,
            worker.clone(),
            native_checkpoint("j", 1, "run", 0, 1, &"x".repeat(257)),
        )
        .await;
        apply(
            &mut jobs,
            3,
            worker.clone(),
            native_checkpoint("j", 1, "run", 2, 4, "snapshot-4"),
        )
        .await;
        for msg in [
            native_checkpoint("j", 1, "run", 2, 4, "conflict"),
            native_checkpoint("j", 1, "run", 3, 3, "stale-revision"),
            native_checkpoint("j", 1, "run", 1, 5, "stale-execution"),
            native_checkpoint("j", 1, "other-run", 0, 5, "wrong-run"),
            native_checkpoint("j", 2, "run", 2, 5, "wrong-claim"),
        ] {
            rejected(&mut jobs, worker.clone(), msg).await;
        }
        rejected(
            &mut jobs,
            ext("intruder"),
            native_checkpoint("j", 1, "run", 2, 4, "snapshot-4"),
        )
        .await;
        apply(
            &mut jobs,
            4,
            worker.clone(),
            native_checkpoint("j", 1, "run", 3, 5, "snapshot-5"),
        )
        .await;
        apply(&mut jobs, 5, worker.clone(), release("j")).await;
        apply(&mut jobs, 6, worker.clone(), claim("j", 100)).await;
        rejected(
            &mut jobs,
            worker.clone(),
            native_checkpoint("j", 1, "run", 3, 5, "snapshot-5"),
        )
        .await;
        rejected(
            &mut jobs,
            worker.clone(),
            native_checkpoint("j", 2, "new-run", 0, 5, "snapshot-5"),
        )
        .await;
        apply(
            &mut jobs,
            7,
            worker.clone(),
            native_checkpoint("j", 2, "new-run", 0, 6, "snapshot-6"),
        )
        .await;
        let head = get(&jobs, "j").await.unwrap().native_history.unwrap();
        assert_eq!(head.job_attempt, 2);
        assert_eq!(head.execution_attempt, 0);
        assert_eq!(head.run_id, "new-run");
        apply(
            &mut jobs,
            8,
            worker.clone(),
            finalize("j", true, "Finished"),
        )
        .await;
        rejected(
            &mut jobs,
            worker,
            native_checkpoint("j", 2, "new-run", 0, 7, "snapshot-7"),
        )
        .await;
        let final_job = get(&jobs, "j").await.unwrap();
        apply(&mut jobs, 9, ext("alice"), prune("j")).await;
        assert_eq!(
            history(&jobs, &final_job.conversation_id).await.executions,
            vec![final_job]
        );
    });
}

#[test]
fn submit_is_one_shot_and_only_explicit_conversations_can_continue() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        for (id, msg, expected) in [
            (
                "one-shot",
                submit("one-shot", "agent", "Same kind"),
                tasks::JobExecution::OneShot,
            ),
            (
                "conversation",
                submit_conversation("conversation", "agent", "Same kind"),
                tasks::JobExecution::Conversation,
            ),
        ] {
            apply(&mut jobs, 1, ext("alice"), msg).await;
            assert_eq!(get(&jobs, id).await.unwrap().execution, expected);
            apply(&mut jobs, 2, ext("alice"), cancel(id)).await;
            apply(&mut jobs, 3, ext("alice"), prune(id)).await;
        }
        rejected(
            &mut jobs,
            ext("alice"),
            continuation("one-shot", "forbidden", "continue", "agent"),
        )
        .await;
        apply(
            &mut jobs,
            4,
            ext("alice"),
            continuation("conversation", "next", "continue", "agent"),
        )
        .await;
        assert_eq!(
            get(&jobs, "next").await.unwrap().execution,
            tasks::JobExecution::Conversation
        );
    });
}

#[test]
fn continuation_is_atomic_bounded_and_never_reuses_an_operation_or_execution_id() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(
            &mut jobs,
            1,
            ext("alice"),
            submit_conversation("j-0", "worker", "Work"),
        )
        .await;
        apply(&mut jobs, 2, ext("alice"), cancel("j-0")).await;
        let first = get(&jobs, "j-0").await.unwrap();
        let root = jobs.root();
        stage(
            &mut jobs,
            3,
            ext("bob"),
            continuation("j-0", "j-1", "continue-1", "worker"),
        )
        .await
        .unwrap();
        assert_eq!(
            history(&jobs, &first.conversation_id).await.executions,
            vec![first.clone()]
        );
        jobs.abort_block().await.unwrap();
        jobs.commit_block().await.unwrap();
        assert_eq!(jobs.root(), root);
        assert!(get(&jobs, "j-1").await.is_none());
        for i in 1..tasks::MAX_WORKER_EXECUTIONS {
            let previous = format!("j-{}", i - 1);
            let next = format!("j-{i}");
            apply(
                &mut jobs,
                4,
                ext("bob"),
                continuation(&previous, &next, &format!("continue-{i}"), "worker"),
            )
            .await;
            apply(&mut jobs, 4, ext("alice"), cancel(&next)).await;
            apply(&mut jobs, 4, ext("alice"), prune(&previous)).await;
        }
        let previous = format!("j-{}", tasks::MAX_WORKER_EXECUTIONS - 1);
        rejected(
            &mut jobs,
            ext("bob"),
            continuation(&previous, "overflow", "new-operation", "worker"),
        )
        .await;
        rejected(
            &mut jobs,
            ext("bob"),
            continuation(&previous, "another", "continue-1", "worker"),
        )
        .await;
        rejected(
            &mut jobs,
            ext("bob"),
            continuation(&previous, "j-0", "unused", "worker"),
        )
        .await;
        let retained = history(&jobs, &first.conversation_id).await;
        assert_eq!(retained.executions.len(), tasks::MAX_WORKER_EXECUTIONS);
        assert_eq!(retained.executions[0], first);
        assert!(
            retained
                .executions
                .iter()
                .all(|job| job.status == JobStatus::Cancelled)
        );
    });
}

#[test]
fn a_queued_comment_cannot_land_on_a_reused_job_id() {
    block_on(async {
        let mut jobs = jobs_on_mem();
        apply(&mut jobs, 1, ext("alice"), submit("reused", "build", "old")).await;
        apply(&mut jobs, 1, ext("alice"), cancel("reused")).await;
        apply(&mut jobs, 1, ext("alice"), prune("reused")).await;
        apply(&mut jobs, 1, ext("alice"), submit("reused", "build", "new")).await;
        let root = jobs.root();
        let error = stage(
            &mut jobs,
            1,
            ext("worker"),
            jobs_msg(JobsMsg::Comment {
                job_id: "reused".into(),
                created_at_revision: 1,
                comment_id: "late".into(),
                text: "Old work".into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(format!("{error}").contains("replaced"));
        jobs.commit_block().await.unwrap();
        assert_eq!(jobs.root(), root);
        assert!(get(&jobs, "reused").await.unwrap().comments.is_empty());
        apply(
            &mut jobs,
            1,
            ext("worker"),
            jobs_msg(JobsMsg::Comment {
                job_id: "reused".into(),
                created_at_revision: 4,
                comment_id: "current".into(),
                text: "New work".into(),
            }),
        )
        .await;
        assert_eq!(
            get(&jobs, "reused").await.unwrap().comments[0].text,
            "New work"
        );
    });
}
