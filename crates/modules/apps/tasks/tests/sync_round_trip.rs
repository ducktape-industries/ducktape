//! state-sync round trip over the REAL store: a joiner reconstructs a
//! byte-identical qmdb root by pulling the source store's operation range
//! through commonware's qmdb sync, then wraps a fresh `Tasks` around the
//! injected store — the sync lane that REPLACED this module's byte snapshot.
//!
//! the source drives ops through the real module so the op log is what a
//! validator produces, and it deliberately carries every shape a naive
//! "export live records and re-apply sorted" could not reproduce: record
//! OVERWRITES (a status change, a claim, a finalize), a record DELETE (`Prune`)
//! and the census/index sentinel keys that ride the same root.
//!
//! a `Tasks` consumes its injected store, so the handoff-as-resolver form is
//! only reachable on the raw store: REOPEN the committed partitions under the
//! same id (exactly the recovery path a restarting node takes — the
//! deterministic runtime shares storage across child contexts).

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use sdk::{Env, MerkleStore as _, Module, Msg, Origin, StateRoot, StateSyncHandle};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;
use tasks::{
    Job, JobStatus, JobsMsg, JobsQuery, JobsReply, Task, TaskMsg, TaskQuery, TaskReply, Tasks,
    decode_job_reply, decode_task_reply, encode_job_msg, encode_job_query, encode_task_msg,
    encode_task_query,
};

const TASKS: &str = "tasks";

fn ctx(height: u64, origin: Origin) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: 1_000 + height,
        origin,
        me: TASKS.into(),
        cause: sdk::Cause::Direct,
    })
    .on_query("identity", |_| {
        Ok(identity::encode_reply(&identity::IdentityReply::Account(
            None,
        )))
    })
}

fn ext(who: &str) -> Origin {
    Origin::External(who.as_bytes().to_vec())
}

/// drive one op through the REAL module path: execute + commit_block (one op
/// per block height), so the committed op log is what a validator produces.
async fn apply(module: &mut Tasks, height: u64, origin: Origin, payload: Vec<u8>) {
    let msg = Msg {
        target: TASKS.into(),
        payload,
    };
    module
        .execute(&mut ctx(height, origin), &msg)
        .await
        .unwrap();
    module.commit_block().await.unwrap();
}

async fn listed(module: &Tasks) -> Vec<Task> {
    let reply = module
        .query(&encode_task_query(&TaskQuery::List {
            limit: tasks::MAX_LIST_LIMIT,
            after: None,
        }))
        .await
        .expect("list");
    let TaskReply::Tasks(tasks) = decode_task_reply(&reply).expect("decode") else {
        panic!("a list answers a page");
    };
    tasks
}

async fn job(module: &Tasks, job_id: &str) -> Option<Job> {
    let reply = module
        .query(&encode_job_query(&JobsQuery::Get {
            job_id: job_id.into(),
        }))
        .await
        .expect("get");
    let JobsReply::Job(job) = decode_job_reply(&reply).expect("decode") else {
        panic!("expected job reply")
    };
    job
}

#[test]
fn synced_store_reconstructs_source_root_and_every_read() {
    deterministic::Runner::default().start(|context| async move {
        let mut src = Tasks::new(
            TASKS,
            "identity",
            "attribution",
            Box::new(QmdbStore::init(context.child("src"), "src").await),
        );

        // task board: two creates (index sentinel grows) plus a status change
        // that OVERWRITES one record.
        apply(
            &mut src,
            1,
            ext("alice"),
            encode_task_msg(&TaskMsg::CreateTask {
                task_id: "zebra".into(),
                title: "Z".into(),
                owner: None,
            }),
        )
        .await;
        apply(
            &mut src,
            2,
            ext("alice"),
            encode_task_msg(&TaskMsg::CreateTask {
                task_id: "alpha".into(),
                title: "A".into(),
                owner: None,
            }),
        )
        .await;
        apply(
            &mut src,
            3,
            ext("alice"),
            encode_task_msg(&TaskMsg::UpdateStatus {
                task_id: "alpha".into(),
                status: tasks::TaskStatus::InProgress,
            }),
        )
        .await;

        // job board: a full lifecycle (overwrites), a worker registration (the
        // worker sentinel), and a prune (a record DELETE in the op log).
        for (id, kind) in [("build", "ci"), ("temp", "tmp")] {
            apply(
                &mut src,
                4,
                ext("alice"),
                encode_job_msg(&JobsMsg::SubmitConversation {
                    job_id: id.into(),
                    kind: kind.into(),
                    spec: format!("spec-{id}"),
                }),
            )
            .await;
        }
        apply(
            &mut src,
            5,
            Origin::Module("agent-fleet".into()),
            encode_job_msg(&JobsMsg::RegisterWorker {}),
        )
        .await;
        apply(
            &mut src,
            6,
            ext("bob"),
            encode_job_msg(&JobsMsg::Claim {
                job_id: "build".into(),
                lease_views: 100,
            }),
        )
        .await;
        apply(
            &mut src,
            7,
            ext("alice"),
            encode_job_msg(&JobsMsg::Cancel {
                job_id: "temp".into(),
            }),
        )
        .await;
        apply(
            &mut src,
            8,
            ext("alice"),
            encode_job_msg(&JobsMsg::Prune {
                job_id: "temp".into(),
            }),
        )
        .await;

        for (origin, msg) in [
            (
                ext("alice"),
                JobsMsg::Control {
                    job_id: "build".into(),
                    operation_id: "steer".into(),
                    input: tasks::JobControlInput::Steer {
                        text: "Retain the evidence".into(),
                    },
                },
            ),
            (
                ext("bob"),
                JobsMsg::AcknowledgeControl {
                    job_id: "build".into(),
                    operation_id: "steer".into(),
                    attempt: 1,
                },
            ),
            (
                ext("bob"),
                JobsMsg::Checkpoint {
                    job_id: "build".into(),
                    operation_id: "checkpoint".into(),
                    attempt: 1,
                    kind: tasks::WorkerReportKind::Checkpoint,
                    payload: "Evidence survived restart".into(),
                },
            ),
            (
                ext("bob"),
                JobsMsg::CheckpointNativeHistory {
                    job_id: "build".into(),
                    attempt: 1,
                    run_id: "run-build".into(),
                    execution_attempt: 2,
                    revision: 77,
                    snapshot: "snapshot-build".into(),
                },
            ),
            (
                ext("alice"),
                JobsMsg::Continue {
                    previous_job_id: "temp".into(),
                    job_id: "temp-next".into(),
                    operation_id: "continue".into(),
                    kind: "tmp".into(),
                    spec: "Continue after pruning".into(),
                },
            ),
        ] {
            apply(&mut src, 9, origin, encode_job_msg(&msg)).await;
        }
        let retained_queries = [
            tasks::encode_job_query(&tasks::JobsQuery::GetWorker {
                conversation_id: "build:1".into(),
            }),
            tasks::encode_job_query(&tasks::JobsQuery::GetWorker {
                conversation_id: "temp:1".into(),
            }),
            tasks::encode_job_query(&tasks::JobsQuery::Controls {
                job_id: "build".into(),
            }),
        ];
        let mut retained_replies = Vec::new();
        for query in &retained_queries {
            retained_replies.push(src.query(query).await.unwrap());
        }

        // the module is resolver-backed: there is NO byte snapshot to ship.
        match src.state_sync_handle().expect("handle") {
            StateSyncHandle::ResolverBacked { backend, .. } => assert_eq!(backend, "qmdb"),
            other => panic!("expected ResolverBacked, got {other:?}"),
        }
        let src_root = src.root();
        assert_ne!(src_root, StateRoot::ZERO, "source must have a real root");
        let src_tasks = listed(&src).await;
        let src_build = job(&src, "build").await.expect("build exists");

        // the module consumed its store, so REOPEN the committed partitions as
        // a bare store for the handoff (drop first — one owner at a time).
        drop(src);
        let src_store = QmdbStore::init(context.child("src_serve"), "src").await;
        assert_eq!(
            src_store.root(),
            src_root,
            "reopened store must recover the committed root"
        );
        let target = src_store.sync_boundary_target().await;
        let resolver = src_store.into_resolver();

        // JOINER: rebuild on a FRESH namespace by pulling the proven op range,
        // then wrap the module around the injected store.
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");
        let mut synced = Tasks::new(TASKS, "identity", "attribution", Box::new(store));

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // and every read answers exactly like the source: the task index rode
        // the sync (ascending order preserved), the claimed job survived, and
        // the pruned record is genuinely gone.
        assert_eq!(listed(&synced).await, src_tasks);
        assert_eq!(src_tasks.len(), 2);
        assert_eq!(src_tasks[0].id, "alpha");
        assert_eq!(src_tasks[0].status, tasks::TaskStatus::InProgress);
        assert_eq!(src_tasks[1].id, "zebra");
        assert_eq!(job(&synced, "build").await, Some(src_build));
        assert_eq!(
            job(&synced, "build").await.unwrap().status,
            JobStatus::Processing
        );
        assert_eq!(job(&synced, "temp").await, None, "the pruned job is gone");
        for (query, expected) in retained_queries.iter().zip(retained_replies) {
            assert_eq!(
                synced.query(query).await.unwrap(),
                expected,
                "retained conversation/controls survive proven state sync"
            );
        }
        assert_eq!(
            job(&synced, "temp-next").await.unwrap().conversation_id,
            "temp:1"
        );
        let mut write_ctx = ctx(9, ext("alice"));
        synced
            .execute(
                &mut write_ctx,
                &Msg {
                    target: TASKS.into(),
                    payload: encode_job_msg(&JobsMsg::Submit {
                        job_id: "temp".into(),
                        kind: "again".into(),
                        spec: "".into(),
                    }),
                },
            )
            .await
            .unwrap();
        let report = write_ctx
            .msgs()
            .iter()
            .find(|message| message.target == "attribution")
            .unwrap();
        let attribution::AttributionMsg::Attribute { revision, .. } =
            attribution::decode_msg(&report.payload).unwrap()
        else {
            panic!("report");
        };
        assert_eq!(
            revision, 4,
            "the deleted object's revision survived state sync"
        );
    });
}
