use attribution::{AttributionMsg, AttributionQuery, AttributionReply, Reason, Source};
use futures::executor::block_on;
use host::{BlockContext, Host};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use sdk_testkit::MemStore;
use sha2::{Digest, Sha256};
use tasks::{JobsMsg, Party, TaskMsg, TaskQuery, TaskReply, TaskStatus, Tasks};

struct Executor;
#[async_trait::async_trait(?Send)]
impl Module for Executor {
    fn id(&self) -> ModuleId {
        "executor".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, _: &mut dyn Ctx, _: &Msg) -> Result<(), Error> {
        Ok(())
    }
}

/// A real queued Attribution subscriber with commit/abort behavior. Tests
/// observe authenticated deliveries here, not Tasks' emitted Attribute intents.
#[derive(Default)]
struct WorkerEvents {
    committed: Vec<attribution::Change>,
    staged: Vec<attribution::Change>,
}

#[async_trait::async_trait(?Send)]
impl Module for WorkerEvents {
    fn id(&self) -> ModuleId {
        "worker-events".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot(Sha256::digest(sdk::wire::encode(&self.committed)).into())
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let from_attribution = ctx.env().origin == Origin::Module("attribution".into());
        if !from_attribution {
            return Err(Error::Module("unauthenticated attribution delivery".into()));
        }
        let attribution::AttributionEvent::Changed(change) =
            attribution::decode_event(&msg.payload).map_err(Error::Module)?;
        self.staged.push(change);
        Ok(())
    }
    async fn query(&self, _: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(sdk::wire::encode(&self.committed))
    }
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.committed.append(&mut self.staged);
        Ok(())
    }
    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.clear();
        Ok(())
    }
}

async fn deliver_worker_events(host: &mut Host) {
    host.submit_block(context(Origin::System), Vec::new())
        .await
        .unwrap();
    assert!(
        !host.has_pending_work().await.unwrap(),
        "the committed delivery batch drained"
    );
}

async fn received_worker_events(host: &Host) -> Vec<attribution::Change> {
    let changes: Vec<attribution::Change> =
        sdk::wire::decode(&host.query("worker-events", &[]).await.unwrap()).unwrap();
    changes
        .into_iter()
        .filter(|change| change.source.kind == "job_event")
        .collect()
}

fn context(origin: Origin) -> BlockContext {
    BlockContext {
        height: 1,
        consensus_time: 1,
        origin,
    }
}
fn alice() -> Origin {
    Origin::External(vec![1; 32])
}
fn bob() -> Origin {
    Origin::External(vec![2; 32])
}

async fn arena() -> Host {
    let mut host = Host::genesis(vec![
        Box::new(Tasks::new(
            "tasks",
            "identity",
            "attribution",
            Box::new(MemStore::new()),
        )),
        Box::new(identity::Identity::new(
            "identity",
            Box::new(MemStore::new()),
            "test".into(),
        )),
        Box::new(attribution::AttributionModule::new(
            "attribution",
            Box::new(MemStore::new()),
        )),
        Box::new(Executor),
    ])
    .unwrap();
    for (origin, name) in [(alice(), "Alice"), (bob(), "Bob")] {
        host.submit_at(
            context(origin),
            Msg {
                target: "identity".into(),
                payload: identity::encode_msg(&identity::IdentityMsg::Create {
                    name: name.into(),
                    scheme: identity::KeyScheme::Ed25519,
                }),
            },
        )
        .await
        .unwrap();
    }
    host.submit_at(
        context(Origin::Module("executor".into())),
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&identity::IdentityMsg::CreateProgram {
                name: "Program".into(),
                controller: 1,
                request: 0,
            }),
        },
    )
    .await
    .unwrap();
    host
}
async fn task(host: &mut Host, origin: Origin, msg: TaskMsg) {
    host.submit_at(
        context(origin),
        Msg {
            target: "tasks".into(),
            payload: tasks::encode_task_msg(&msg),
        },
    )
    .await
    .unwrap();
}
async fn job(host: &mut Host, origin: Origin, msg: JobsMsg) {
    host.submit_at(
        context(origin),
        Msg {
            target: "tasks".into(),
            payload: tasks::encode_job_msg(&msg),
        },
    )
    .await
    .unwrap();
}
async fn relations(host: &Host, kind: &str, object: &str) -> attribution::ObjectRelations {
    let query = AttributionQuery::Relations {
        source: Source {
            module: "tasks".into(),
            kind: kind.into(),
            object: object.into(),
        },
    };
    let bytes = host
        .query("attribution", &attribution::encode_query(&query))
        .await
        .unwrap();
    let AttributionReply::Relations(Some(relations)) = attribution::decode_reply(&bytes).unwrap()
    else {
        panic!("source relations");
    };
    relations
}
fn create() -> TaskMsg {
    TaskMsg::CreateTask {
        task_id: "task".into(),
        title: "Work".into(),
        owner: None,
    }
}

#[test]
fn program_owns_ordinary_tasks_and_recreation_retains_revision_history() {
    block_on(async {
        let mut host = arena().await;
        task(&mut host, Origin::Program(3), create()).await;
        let source = relations(&host, "task", "task").await;
        assert_eq!(source.revision, 1);
        assert_eq!(source.relations[0].recipient, 3);
        let query = tasks::encode_task_query(&TaskQuery::Get {
            task_id: "task".into(),
        });
        let TaskReply::Task(Some(record)) =
            tasks::decode_task_reply(&host.query("tasks", &query).await.unwrap()).unwrap()
        else {
            panic!("task");
        };
        assert_eq!(record.owner, Party::Account(3));
        task(
            &mut host,
            Origin::Program(3),
            TaskMsg::DeleteTask {
                task_id: "task".into(),
            },
        )
        .await;
        let retired = relations(&host, "task", "task").await;
        assert_eq!(retired.revision, 2);
        assert!(retired.relations.is_empty());
        task(&mut host, Origin::Program(3), create()).await;
        assert_eq!(relations(&host, "task", "task").await.revision, 3);
        let query = AttributionQuery::ChangesOf {
            source: source.source,
            after: 0,
            limit: 100,
        };
        let bytes = host
            .query("attribution", &attribution::encode_query(&query))
            .await
            .unwrap();
        let AttributionReply::Changes(changes) = attribution::decode_reply(&bytes).unwrap() else {
            panic!("changes");
        };
        assert_eq!(changes.len(), 3);
        assert!(
            changes
                .iter()
                .all(|entry| entry.change.actor == attribution::Actor::Account(3))
        );
    });
}

#[test]
fn claims_release_and_results_publish_full_relation_sets() {
    block_on(async {
        let mut host = arena().await;
        job(
            &mut host,
            alice(),
            JobsMsg::Submit {
                job_id: "job".into(),
                kind: "review".into(),
                spec: "Check".into(),
            },
        )
        .await;
        job(
            &mut host,
            Origin::Program(3),
            JobsMsg::Claim {
                job_id: "job".into(),
                lease_views: 10,
            },
        )
        .await;
        assert!(
            relations(&host, "job", "job")
                .await
                .relations
                .iter()
                .any(|relation| relation.recipient == 3 && relation.reason == Reason::Assignment)
        );
        job(
            &mut host,
            Origin::Program(3),
            JobsMsg::Release {
                job_id: "job".into(),
            },
        )
        .await;
        assert!(
            !relations(&host, "job", "job")
                .await
                .relations
                .iter()
                .any(|relation| relation.reason == Reason::Assignment)
        );
        job(
            &mut host,
            bob(),
            JobsMsg::Claim {
                job_id: "job".into(),
                lease_views: 10,
            },
        )
        .await;
        job(
            &mut host,
            bob(),
            JobsMsg::Finalize {
                job_id: "job".into(),
                ok: false,
                payload: "Review failed".into(),
            },
        )
        .await;
        let completed = relations(&host, "job", "job").await;
        assert_eq!(completed.revision, 5);
        assert!(
            completed
                .relations
                .iter()
                .any(|relation| relation.recipient == 1 && relation.reason == Reason::Result)
        );
        assert!(
            completed
                .relations
                .iter()
                .any(|relation| relation.recipient == 2 && relation.reason == Reason::Assignment)
        );
        job(
            &mut host,
            alice(),
            JobsMsg::Prune {
                job_id: "job".into(),
            },
        )
        .await;
        assert!(relations(&host, "job", "job").await.relations.is_empty());
        job(
            &mut host,
            alice(),
            JobsMsg::Submit {
                job_id: "job".into(),
                kind: "again".into(),
                spec: "".into(),
            },
        )
        .await;
        assert_eq!(relations(&host, "job", "job").await.revision, 7);
    });
}

#[test]
fn every_semantic_worker_event_is_delivered_once_from_an_immutable_source() {
    block_on(async {
        let mut host = arena().await;
        host.register(Box::new(WorkerEvents::default()));
        host.submit_at(
            context(Origin::Module("worker-events".into())),
            Msg {
                target: "attribution".into(),
                payload: attribution::encode_msg(&AttributionMsg::Subscribe {}),
            },
        )
        .await
        .unwrap();
        job(
            &mut host,
            alice(),
            JobsMsg::Submit {
                job_id: "ordinary".into(),
                kind: "research".into(),
                spec: "One-shot work".into(),
            },
        )
        .await;
        deliver_worker_events(&mut host).await;
        assert!(
            received_worker_events(&host).await.is_empty(),
            "ordinary Submit has no native job_event"
        );
        job(
            &mut host,
            bob(),
            JobsMsg::Claim {
                job_id: "ordinary".into(),
                lease_views: 100,
            },
        )
        .await;
        job(
            &mut host,
            bob(),
            JobsMsg::Finalize {
                job_id: "ordinary".into(),
                ok: true,
                payload: "One-shot result".into(),
            },
        )
        .await;
        deliver_worker_events(&mut host).await;
        assert!(
            received_worker_events(&host).await.is_empty(),
            "ordinary lifecycle has no native event deliveries"
        );
        let ordinary = relations(&host, "job", "ordinary").await;
        assert_eq!(ordinary.revision, 3);
        for (recipient, reason) in [
            (1, Reason::Authorship),
            (1, Reason::Ownership),
            (2, Reason::Assignment),
            (1, Reason::Result),
        ] {
            assert!(
                ordinary
                    .relations
                    .iter()
                    .any(|relation| relation.recipient == recipient && relation.reason == reason)
            );
        }
        let delivered: Vec<attribution::Change> =
            sdk::wire::decode(&host.query("worker-events", &[]).await.unwrap()).unwrap();
        assert!(
            delivered.iter().any(|change| change.source.kind == "job"
                && change.source.object == "ordinary"
                && change.reason == Reason::Result
                && change.recipient == 1),
            "ordinary Result still reaches subscribers on its original source"
        );
        job(
            &mut host,
            alice(),
            JobsMsg::SubmitConversation {
                job_id: "worker".into(),
                kind: "research".into(),
                spec: "Investigate".into(),
            },
        )
        .await;
        job(
            &mut host,
            Origin::Module("executor".into()),
            JobsMsg::Claim {
                job_id: "worker".into(),
                lease_views: 100,
            },
        )
        .await;
        deliver_worker_events(&mut host).await;
        assert_eq!(received_worker_events(&host).await.len(), 1);
        let attribution_root = host.module_root("attribution").unwrap();
        job(
            &mut host,
            Origin::Module("executor".into()),
            JobsMsg::CheckpointNativeHistory {
                job_id: "worker".into(),
                attempt: 1,
                run_id: "run-worker".into(),
                execution_attempt: 0,
                revision: 1,
                snapshot: "snapshot-worker".into(),
            },
        )
        .await;
        deliver_worker_events(&mut host).await;
        assert_eq!(host.module_root("attribution").unwrap(), attribution_root);
        assert_eq!(relations(&host, "job", "worker").await.revision, 2);
        let updates = [
            (
                Origin::Module("executor".into()),
                JobsMsg::Checkpoint {
                    job_id: "worker".into(),
                    operation_id: "first".into(),
                    attempt: 1,
                    kind: tasks::WorkerReportKind::Checkpoint,
                    payload: "Started investigation".into(),
                },
                "worker_checkpoint",
            ),
            (
                Origin::Module("executor".into()),
                JobsMsg::Checkpoint {
                    job_id: "worker".into(),
                    operation_id: "blocker".into(),
                    attempt: 1,
                    kind: tasks::WorkerReportKind::Checkpoint,
                    payload: "Blocked: need the source data".into(),
                },
                "worker_checkpoint",
            ),
            (
                Origin::Module("executor".into()),
                JobsMsg::Checkpoint {
                    job_id: "worker".into(),
                    operation_id: "report".into(),
                    attempt: 1,
                    kind: tasks::WorkerReportKind::Report,
                    payload: "Evidence retained".into(),
                },
                "worker_report",
            ),
            (
                bob(),
                JobsMsg::Control {
                    job_id: "worker".into(),
                    operation_id: "steer".into(),
                    input: tasks::JobControlInput::Steer {
                        text: "Use another source".into(),
                    },
                },
                "worker_control",
            ),
            (
                bob(),
                JobsMsg::Control {
                    job_id: "worker".into(),
                    operation_id: "cancel".into(),
                    input: tasks::JobControlInput::Cancel,
                },
                "worker_control",
            ),
            (
                Origin::Module("executor".into()),
                JobsMsg::AcknowledgeControl {
                    job_id: "worker".into(),
                    operation_id: "steer".into(),
                    attempt: 1,
                },
                "worker_control_acknowledged",
            ),
            (
                Origin::Module("executor".into()),
                JobsMsg::AcknowledgeControl {
                    job_id: "worker".into(),
                    operation_id: "cancel".into(),
                    attempt: 1,
                },
                "worker_control_acknowledged",
            ),
        ];
        for (offset, (origin, operation, reason)) in updates.into_iter().enumerate() {
            job(&mut host, origin.clone(), operation.clone()).await;
            deliver_worker_events(&mut host).await;
            let received = received_worker_events(&host).await;
            assert_eq!(
                received.len(),
                offset + 2,
                "every semantic update reaches the subscriber"
            );
            let change = received.last().unwrap();
            assert_eq!(change.recipient, 1);
            assert_eq!(change.reason, Reason::Defined(reason.into()));
            assert_eq!(change.kind, attribution::ChangeKind::Added);
            let detail: tasks::JobEventDetail = sdk::wire::decode(&change.detail).unwrap();
            assert_eq!(detail.job_id, "worker");
            assert_eq!(detail.conversation_id, "worker:1");
            assert_eq!(detail.job_kind, "research");
            assert_eq!(detail.created_at_revision, 1);
            assert_eq!(detail.job_attempt, 1);
            assert_eq!(detail.submitter, Party::Account(1));
            assert_eq!(detail.actor, change.actor);
            assert_eq!(detail.height, change.height);
            assert_eq!(detail.operation, operation);
            let source_root = host.module_root("attribution").unwrap();
            let subscriber_root = host.module_root("worker-events").unwrap();
            job(&mut host, origin, operation).await;
            deliver_worker_events(&mut host).await;
            assert_eq!(
                host.module_root("attribution").unwrap(),
                source_root,
                "exact retry produces no new source revision or change"
            );
            assert_eq!(
                host.module_root("worker-events").unwrap(),
                subscriber_root,
                "exact retry produces no delivery"
            );
        }
        job(
            &mut host,
            Origin::Module("executor".into()),
            JobsMsg::Release {
                job_id: "worker".into(),
            },
        )
        .await;
        job(
            &mut host,
            Origin::Module("executor".into()),
            JobsMsg::Claim {
                job_id: "worker".into(),
                lease_views: 100,
            },
        )
        .await;
        job(
            &mut host,
            Origin::Module("executor".into()),
            JobsMsg::AcknowledgeControl {
                job_id: "worker".into(),
                operation_id: "cancel".into(),
                attempt: 2,
            },
        )
        .await;
        deliver_worker_events(&mut host).await;
        let received = received_worker_events(&host).await;
        assert_eq!(
            received.len(),
            9,
            "the same control acknowledged by a new attempt has its own source"
        );
        let detail: tasks::JobEventDetail =
            sdk::wire::decode(&received.last().unwrap().detail).unwrap();
        assert_eq!(detail.job_attempt, 2);
        job(
            &mut host,
            Origin::Module("executor".into()),
            JobsMsg::SettleCancellation {
                job_id: "worker".into(),
                operation_id: "cancel".into(),
                attempt: 2,
                payload: "Stopped safely".into(),
            },
        )
        .await;
        let settled = relations(&host, "job", "worker").await;
        assert!(
            settled
                .relations
                .iter()
                .any(|relation| relation.recipient == 1 && relation.reason == Reason::Result)
        );
        assert!(
            !settled
                .relations
                .iter()
                .any(|relation| matches!(relation.reason, Reason::Defined(_))),
            "mutable Job relations are not semantic event sources"
        );
        job(
            &mut host,
            bob(),
            JobsMsg::Prune {
                job_id: "worker".into(),
            },
        )
        .await;
        job(
            &mut host,
            bob(),
            JobsMsg::Continue {
                previous_job_id: "worker".into(),
                job_id: "next".into(),
                operation_id: "continue".into(),
                kind: "writer".into(),
                spec: "More".into(),
            },
        )
        .await;
        deliver_worker_events(&mut host).await;
        let received = received_worker_events(&host).await;
        assert_eq!(received.len(), 11);
        let continued: tasks::JobEventDetail =
            sdk::wire::decode(&received.last().unwrap().detail).unwrap();
        assert_eq!(continued.job_id, "next");
        assert_eq!(continued.job_kind, "writer");
        assert_eq!(continued.conversation_id, "worker:1");
        assert_eq!(continued.submitter, Party::Account(1));
        for (reason, expected) in [
            ("worker_checkpoint", 2),
            ("worker_report", 1),
            ("worker_control", 2),
            ("worker_control_acknowledged", 3),
        ] {
            assert_eq!(
                received
                    .iter()
                    .filter(|change| change.reason == Reason::Defined(reason.into()))
                    .count(),
                expected
            );
        }
        let sources = received
            .iter()
            .map(|change| change.source.clone())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(sources.len(), received.len());
        for change in &received {
            assert_eq!(change.revision, 1);
            assert_eq!(change.source.module, "tasks");
            assert_eq!(change.source.object.len(), 64);
            assert_eq!(
                change.kind,
                attribution::ChangeKind::Added,
                "pruning a Job never withdraws its event sources"
            );
            let immutable = relations(&host, "job_event", &change.source.object).await;
            assert_eq!(immutable.revision, 1);
            assert_eq!(immutable.changes, 1);
            assert_eq!(immutable.relations[0].detail, change.detail);
        }
        let bytes = host
            .query(
                "attribution",
                &attribution::encode_query(&AttributionQuery::ChangesFor {
                    recipient: 1,
                    after: 0,
                    limit: 100,
                }),
            )
            .await
            .unwrap();
        let AttributionReply::Changes(changes) = attribution::decode_reply(&bytes).unwrap() else {
            panic!("changes");
        };
        let canonical = changes
            .into_iter()
            .map(|entry| entry.change)
            .filter(|change| change.source.kind == "job_event")
            .collect::<Vec<_>>();
        assert_eq!(
            canonical, received,
            "subscriber received each canonical immutable change, including the later blocker"
        );
        let bytes = host
            .query(
                "attribution",
                &attribution::encode_query(&AttributionQuery::DeliveriesOf {
                    subscriber: "worker-events".into(),
                    after: 0,
                    limit: 100,
                }),
            )
            .await
            .unwrap();
        let AttributionReply::Deliveries(deliveries) = attribution::decode_reply(&bytes).unwrap()
        else {
            panic!("deliveries");
        };
        let event_seqs = received
            .iter()
            .map(|change| change.seq)
            .collect::<std::collections::BTreeSet<_>>();
        let receipts = deliveries
            .into_iter()
            .filter(|entry| event_seqs.contains(&entry.delivery.seq))
            .collect::<Vec<_>>();
        assert_eq!(receipts.len(), received.len());
        assert!(receipts.iter().all(|entry| entry.delivery.state
            == attribution::DeliveryState::Retired(sdk::DeliveryOutcome::Applied)));
    });
}

#[test]
fn attribution_rejection_rolls_back_the_source_and_assigned_output() {
    block_on(async {
        let mut host = arena().await;
        host.submit_at(
            context(Origin::Module("tasks".into())),
            Msg {
                target: "attribution".into(),
                payload: attribution::encode_msg(&AttributionMsg::Attribute {
                    object: attribution::ObjectRef {
                        kind: "task".into(),
                        object: "task".into(),
                    },
                    revision: 1,
                    actor: attribution::Actor::System,
                    relations: Vec::new(),
                    transfers: Vec::new(),
                }),
            },
        )
        .await
        .unwrap();
        let root = host.root_hash();
        let result = host
            .submit_at(
                context(alice()),
                Msg {
                    target: "tasks".into(),
                    payload: tasks::encode_task_msg(&create()),
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(host.root_hash(), root);
        let bytes = host
            .query(
                "tasks",
                &tasks::encode_task_query(&TaskQuery::Get {
                    task_id: "task".into(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            tasks::decode_task_reply(&bytes).unwrap(),
            TaskReply::Task(None)
        );
    });
}

#[test]
fn an_authenticated_key_keeps_its_records_after_joining_identity() {
    block_on(async {
        let mut host = arena().await;
        let signer = Origin::External(vec![4; 32]);
        task(&mut host, signer.clone(), create()).await;
        job(
            &mut host,
            signer.clone(),
            JobsMsg::Submit {
                job_id: "owned".into(),
                kind: "work".into(),
                spec: "".into(),
            },
        )
        .await;
        job(
            &mut host,
            alice(),
            JobsMsg::Submit {
                job_id: "claimed".into(),
                kind: "work".into(),
                spec: "".into(),
            },
        )
        .await;
        job(
            &mut host,
            signer.clone(),
            JobsMsg::Claim {
                job_id: "claimed".into(),
                lease_views: 10,
            },
        )
        .await;
        host.submit_at(
            context(signer.clone()),
            Msg {
                target: "identity".into(),
                payload: identity::encode_msg(&identity::IdentityMsg::Create {
                    name: "Joined".into(),
                    scheme: identity::KeyScheme::Ed25519,
                }),
            },
        )
        .await
        .unwrap();
        task(
            &mut host,
            signer.clone(),
            TaskMsg::UpdateStatus {
                task_id: "task".into(),
                status: TaskStatus::Done,
            },
        )
        .await;
        job(
            &mut host,
            signer.clone(),
            JobsMsg::Cancel {
                job_id: "owned".into(),
            },
        )
        .await;
        job(
            &mut host,
            signer.clone(),
            JobsMsg::Prune {
                job_id: "owned".into(),
            },
        )
        .await;
        job(
            &mut host,
            signer.clone(),
            JobsMsg::Finalize {
                job_id: "claimed".into(),
                ok: true,
                payload: "Finished after joining".into(),
            },
        )
        .await;
        let query = tasks::encode_task_query(&TaskQuery::Get {
            task_id: "task".into(),
        });
        let TaskReply::Task(Some(record)) =
            tasks::decode_task_reply(&host.query("tasks", &query).await.unwrap()).unwrap()
        else {
            panic!("task");
        };
        assert_eq!(
            record.owner,
            Party::Key(vec![4; 32]),
            "admission never transfers ownership"
        );
        assert_eq!(record.status, TaskStatus::Done);
        task(
            &mut host,
            signer,
            TaskMsg::DeleteTask {
                task_id: "task".into(),
            },
        )
        .await;
    });
}
