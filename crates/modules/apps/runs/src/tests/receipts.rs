use super::*;
use std::rc::Rc;

#[derive(Default)]
struct Stored {
    records: BTreeMap<[u8; 32], Vec<u8>>,
    reads: usize,
    writes: Vec<[u8; 32]>,
}

#[derive(Clone, Default)]
struct Backing(Rc<RefCell<Stored>>);

#[async_trait::async_trait(?Send)]
impl sdk::MerkleStore for Backing {
    async fn get(&self, key: &[u8; 32]) -> Result<Option<Vec<u8>>, Error> {
        let mut stored = self.0.borrow_mut();
        stored.reads += 1;
        Ok(stored.records.get(key).cloned())
    }
    async fn commit_batch(
        &mut self,
        writes: Vec<([u8; 32], Option<Vec<u8>>)>,
    ) -> Result<(), Error> {
        let mut stored = self.0.borrow_mut();
        for (key, value) in writes {
            stored.writes.push(key);
            match value {
                Some(value) => {
                    stored.records.insert(key, value);
                }
                None => {
                    stored.records.remove(&key);
                }
            }
        }
        Ok(())
    }
    fn root(&self) -> StateRoot {
        panic!("the host owns the receipt backing root")
    }
    async fn sync_target(&self) -> Result<sdk::ResolverSyncTarget, Error> {
        Err(Error::SyncUnsupported)
    }
    async fn serve_sync(&self, _: &[u8]) -> Result<Vec<u8>, Error> {
        Err(Error::SyncUnsupported)
    }
}

fn hosted() -> (RunsModule, Backing, PendingState) {
    let (module, _, run_id) = awaiting_run();
    let entry = module
        .pending_entry(&dispatch_id_for(&run_id))
        .unwrap()
        .clone();
    let backing = Backing::default();
    (
        module.with_receipt_store(Box::new(backing.clone())),
        backing,
        entry,
    )
}

fn stage(module: &mut RunsModule, entry: &PendingState, slot: u32) -> String {
    let id = action_request_id(&entry.run_id, &format!("slot-{slot}"));
    block_on(module.stage_action_request(
        entry,
        id.clone(),
        crate::action_requests::RequestScope::Result,
        crate::action_requests::Prepared::new(
            Msg {
                target: "tasks".into(),
                payload: tasks::encode_task_msg(&TaskMsg::CreateTask {
                    task_id: format!("task-{slot}"),
                    title: "A durable proposal".into(),
                    owner: None,
                }),
            },
            OP_TASKS_CREATE,
            serde_json::json!({"task_id": format!("task-{slot}")}),
        ),
    ))
    .unwrap();
    id
}

fn acknowledge(module: &mut RunsModule, item: u64, outcome: sdk::DeliveryOutcome) {
    let ctx = CaptureCtx::new().with_origin(Origin::System);
    block_on(module.acknowledge_action(
        &ctx,
        &sdk::Ack {
            item,
            target: "runs".into(),
            outcome,
        },
    ))
    .unwrap();
}

#[test]
fn result_action_rejection_is_committed_sticky_and_does_not_replace_worker_failure() {
    let (mut module, _, entry) = hosted();
    let id = stage(&mut module, &entry, 0);
    let request = block_on(module.action_request(&id)).unwrap().unwrap();
    module.pending_history.push(RunRecord {
        run_id: entry.run_id.clone(),
        agent_id: entry.agent_id.clone(),
        channel_id: entry.channel_id.clone(),
        anchor_seq: entry.anchor_seq,
        outcome: RunOutcome::ResultAccepted,
        degraded: false,
        created_at: 1,
        delivered_at: 2,
        executing_node: "node".into(),
        output_ref: None,
        pr: None,
    });
    commit(&mut module);
    let rejection = dispatch::CallOutcomeSummary::Rejected {
        reason: "target refused".into(),
    };
    let applied = dispatch::CallOutcomeSummary::Applied {
        output_digest: [0; 32],
        assigned: Vec::new(),
    };
    module.stage_completed_action_outcome(&request, &rejection);
    assert_eq!(recent_runs(&module)[0].outcome, RunOutcome::ResultAccepted);
    abort(&mut module);
    commit(&mut module);
    assert_eq!(recent_runs(&module)[0].outcome, RunOutcome::ResultAccepted);
    module.stage_completed_action_outcome(&request, &rejection);
    module.stage_completed_action_outcome(&request, &applied);
    commit(&mut module);
    assert_eq!(recent_runs(&module)[0].outcome, RunOutcome::ActionRejected);
    module.stage_completed_action_outcome(&request, &applied);
    commit(&mut module);
    assert_eq!(recent_runs(&module)[0].outcome, RunOutcome::ActionRejected);
    module.history.front_mut().unwrap().outcome = RunOutcome::Failed;
    module.stage_completed_action_outcome(&request, &rejection);
    commit(&mut module);
    assert_eq!(recent_runs(&module)[0].outcome, RunOutcome::Failed);
}

#[test]
fn a_verified_pr_link_is_staged_and_abort_discards_it() {
    let (mut module, _, entry) = hosted();
    module = module.with_sink_forge("forge");
    let id = "pr-rollback".to_string();
    block_on(module.stage_action_request(
        &entry,
        id.clone(),
        crate::action_requests::RequestScope::Result,
        crate::action_requests::Prepared::new(
            Msg {
                target: "forge".into(),
                payload: sdk::wire::encode(&serde_json::json!({"open_pr":{"repo":"demo"}})),
            },
            "forge.open_pr",
            serde_json::Value::Null,
        ),
    ))
    .unwrap();
    let request = block_on(module.action_request(&id)).unwrap().unwrap();
    module.pending_history.push(RunRecord {
        run_id: entry.run_id.clone(),
        agent_id: entry.agent_id.clone(),
        channel_id: entry.channel_id.clone(),
        anchor_seq: entry.anchor_seq,
        outcome: RunOutcome::ResultAccepted,
        degraded: false,
        created_at: 1,
        delivered_at: 2,
        executing_node: "node".into(),
        output_ref: None,
        pr: None,
    });
    commit(&mut module);
    let output = serde_json::json!({"number":7,"repo":"demo"});
    let outcome = dispatch::CallOutcomeSummary::Applied {
        output_digest: Sha256::digest(sdk::wire::encode(&output)).into(),
        assigned: Vec::new(),
    };
    let result = agent::CallResult::Applied {
        output,
        assigned: serde_json::Value::Null,
    };
    module
        .stage_completed_pr_link(&request, &outcome, result.clone())
        .unwrap();
    assert_eq!(
        recent_runs(&module)[0].pr,
        None,
        "completion is still uncommitted"
    );
    abort(&mut module);
    commit(&mut module);
    assert_eq!(
        recent_runs(&module)[0].pr,
        None,
        "abort left no link behind"
    );
    module
        .stage_completed_pr_link(&request, &outcome, result.clone())
        .unwrap();
    module
        .stage_completed_pr_link(&request, &outcome, result)
        .unwrap();
    assert_eq!(
        recent_runs(&module)[0].pr,
        None,
        "an exact repeat still waits for commit"
    );
    commit(&mut module);
    assert_eq!(
        recent_runs(&module)[0].pr,
        Some(PrRef {
            repo: "demo".into(),
            number: 7
        })
    );
    assert_eq!(
        recent_runs(&module).len(),
        1,
        "retries do not append duplicate history"
    );
}

#[test]
fn reordered_json_has_the_same_durable_proposal_bytes() {
    let original =
        br#"{"task":{"create_task":{"task_id":"same","title":"Same task","owner":null}}}"#;
    let reordered =
        br#"{"task":{"create_task":{"owner":null,"title":"Same task","task_id":"same"}}}"#;
    let mut snapshots = Vec::new();
    for payload in [original.as_slice(), reordered.as_slice()] {
        let (mut module, backing, entry) = hosted();
        block_on(module.stage_action_request(
            &entry,
            "reordered".into(),
            crate::action_requests::RequestScope::Result,
            crate::action_requests::Prepared::new(
                Msg {
                    target: "tasks".into(),
                    payload: payload.to_vec(),
                },
                OP_TASKS_CREATE,
                serde_json::json!({"task_id": "same"}),
            ),
        ))
        .unwrap();
        commit(&mut module);
        let request = block_on(module.action_request("reordered"))
            .unwrap()
            .unwrap();
        assert_eq!(
            sdk::wire::encode(&request.view.payload),
            br#"{"task":{"create_task":{"owner":null,"task_id":"same","title":"Same task"}}}"#
        );
        snapshots.push(backing.0.borrow().records.clone());
    }
    assert_eq!(
        snapshots[0], snapshots[1],
        "field order cannot alter the persisted proposal or its call digest"
    );
}

#[test]
fn receipt_history_does_not_increase_one_actions_reads_or_writes() {
    let (mut module, backing, entry) = hosted();
    let history = sdk::MAX_DELIVERIES_PER_BLOCK * 4;
    for slot in 0..history {
        stage(&mut module, &entry, slot as u32);
    }
    commit(&mut module);
    loop {
        let batch = block_on(module.action_deliveries()).unwrap();
        if batch.is_empty() {
            break;
        }
        assert!(batch.len() <= sdk::MAX_DELIVERIES_PER_BLOCK);
        for item in batch {
            acknowledge(&mut module, item.item, sdk::DeliveryOutcome::Applied);
        }
        commit(&mut module);
    }
    let control = module.snapshot();
    backing.0.borrow_mut().reads = 0;
    backing.0.borrow_mut().writes.clear();
    let id = stage(&mut module, &entry, history as u32);
    commit(&mut module);
    let stored = backing.0.borrow();
    assert_eq!(
        stored.reads, 3,
        "one id lookup, empty queue metadata, and the optional retained-conversation binding"
    );
    assert_eq!(
        stored.writes.len(),
        4,
        "body, reserved marker, item and queue"
    );
    assert_eq!(
        module.snapshot().len(),
        control.len(),
        "receipt history is outside the control blob"
    );
    drop(stored);
    let mut restored = super::module().with_receipt_store(Box::new(backing.clone()));
    restored.install(&module.snapshot(), module.root()).unwrap();
    assert_eq!(
        block_on(restored.action_request(&id))
            .unwrap()
            .unwrap()
            .view
            .request_id,
        id
    );
    assert_eq!(
        block_on(restored.action_deliveries()).unwrap(),
        block_on(module.action_deliveries()).unwrap()
    );
}

#[test]
fn terminal_marker_has_its_full_capacity_before_execution() {
    let (mut module, backing, entry) = hosted();
    let id = stage(&mut module, &entry, 0);
    commit(&mut module);
    let marker_key = sdk::store_key(format!("action/marker/{id}").as_bytes());
    let before = backing.0.borrow().records[&marker_key].len();
    let body_key = sdk::store_key(format!("action/body/{id}").as_bytes());
    let body = backing.0.borrow().records[&body_key].clone();
    let mut request = block_on(module.action_request(&id)).unwrap().unwrap();
    request.view.status = ActionStatus::Completed {
        call: sdk::CallId {
            requester: "agent".into(),
            invocation: format!("{}/{}", u64::MAX, u64::MAX),
            step: u64::MAX,
        },
        outcome: dispatch::CallOutcomeSummary::Applied {
            output_digest: [255; 32],
            assigned: vec![255; sdk::MAX_ASSIGNED_BYTES],
        },
    };
    block_on(module.stage_action_marker(&request)).unwrap();
    commit(&mut module);
    let stored = backing.0.borrow();
    assert_eq!(stored.records[&marker_key].len(), before);
    assert_eq!(
        stored.records[&body_key], body,
        "completion does not rewrite the proposal"
    );
}

#[test]
fn oversized_ack_diagnostic_cannot_strand_the_reserved_marker_or_queue() {
    let (mut module, _, entry) = hosted();
    let id = stage(&mut module, &entry, 0);
    commit(&mut module);
    let item = block_on(module.action_deliveries()).unwrap().remove(0).item;
    let outcome = sdk::DeliveryOutcome::Failed {
        reason: "x".repeat(sdk::MAX_STORE_VALUE_BYTES + 1),
    };
    acknowledge(&mut module, item, outcome.clone());
    commit(&mut module);
    assert!(block_on(module.action_deliveries()).unwrap().is_empty());
    assert!(matches!(
        block_on(module.action_request(&id))
            .unwrap()
            .unwrap()
            .view
            .status,
        ActionStatus::Rejected { .. }
    ));
    acknowledge(&mut module, item, outcome);
    let ctx = CaptureCtx::new().with_origin(Origin::System);
    assert!(
        block_on(module.acknowledge_action(
            &ctx,
            &sdk::Ack {
                item,
                target: "runs".into(),
                outcome: sdk::DeliveryOutcome::Applied
            }
        ))
        .is_err()
    );
}

#[test]
fn aborted_admission_leaves_no_receipt_or_queue_record() {
    let (mut module, backing, entry) = hosted();
    let before = module.snapshot();
    let id = stage(&mut module, &entry, 0);
    abort(&mut module);
    assert_eq!(module.snapshot(), before);
    assert!(backing.0.borrow().records.is_empty());
    assert!(block_on(module.action_request(&id)).unwrap().is_none());
}
