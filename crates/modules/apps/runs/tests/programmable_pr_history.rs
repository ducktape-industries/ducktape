mod support;

use futures::executor::block_on;
use support::*;

struct Directory(std::path::PathBuf);
impl Directory {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!("runs-pr-history-{label}-{}", std::process::id())))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn push(branch: &str, previous: Option<u8>, next: Option<u8>) -> forge::ForgeMsg {
    forge::ForgeMsg::PushRefs {
        repo: "demo".into(),
        updates: vec![forge::RefUpdate {
            ref_name: branch.into(),
            prev_oid: previous.map(|byte| vec![byte; 20]),
            new_oid: next.map(|byte| vec![byte; 20]),
        }],
        pack_digest: next.map(|_| vec![9; 32]),
        cert: None,
    }
}

async fn issue(network: &mut Network, title: &str) {
    network
        .submit(
            member(),
            msg(
                "forge",
                &forge::ForgeMsg::OpenIssue {
                    repo: "demo".into(),
                    title: title.into(),
                    body: String::new(),
                },
            ),
        )
        .await;
}

async fn history(network: &Network, run: &str) -> Option<runs::RunRecord> {
    let bytes = network
        .host
        .query("runs", &runs::encode_query(&runs::RunsQuery::RecentRuns))
        .await
        .unwrap();
    let runs::RunsReply::RecentRuns(records) = runs::decode_reply(&bytes).unwrap() else {
        panic!("history");
    };
    records.into_iter().find(|record| record.run_id == run)
}

async fn item(network: &Network, number: u64) -> Option<Box<forge::ItemDetail>> {
    let bytes = network
        .host
        .query(
            "forge",
            &forge::encode_query(&forge::ForgeQuery::GetItem {
                repo: "demo".into(),
                number,
            }),
        )
        .await
        .unwrap();
    let forge::ForgeReply::Item(item) = forge::decode_reply(&bytes).unwrap() else {
        panic!("item");
    };
    item
}

async fn awaiting_pr(
    directory: &Directory,
    program: agent::Program,
) -> (Network, runs::PendingRun) {
    awaiting_pr_with_actions(directory, program, Vec::new()).await
}

async fn awaiting_pr_with_actions(
    directory: &Directory,
    program: agent::Program,
    actions: Vec<runs::ActionEnvelope>,
) -> (Network, runs::PendingRun) {
    let mut network = Network::new().await;
    network.host.register(Box::new(
        forge::Forge::init("forge", &directory.0)
            .unwrap()
            .with_chat("chat")
            .with_attribution("attribution")
            .with_chain_id("runs-test"),
    ));
    network.provision_program(program).await;
    network
        .submit(member(), msg("forge", &push("dev", None, Some(1))))
        .await;
    network
        .submit(
            member(),
            msg("forge", &push("agent/item-1", None, Some(0x1a))),
        )
        .await;
    issue(&mut network, "Implement the change").await;
    network
        .submit(
            member(),
            msg(
                "chat",
                &chat::ChatMsg::PostMessage {
                    channel_id: "forge:demo:1".into(),
                    message_id: "forge-anchor".into(),
                    thread: None,
                    blocks: vec![chat::Block::paragraph("Implement this issue")],
                },
            ),
        )
        .await;
    network
        .submit(
            member(),
            msg(
                "runs",
                &runs::RunsMsg::RequestRun {
                    agent_id: "builder".into(),
                    channel_id: "forge:demo:1".into(),
                    anchor_seq: 1,
                    demands: Default::default(),
                    skills: Vec::new(),
                },
            ),
        )
        .await;
    network.drain().await;
    let pending = network.runs().await;
    let Some(run) = pending
        .into_iter()
        .find(|run| run.channel_id == "forge:demo:1")
    else {
        let invocations = network
            .host
            .query(
                "agent",
                &agent::encode_query(&agent::AgentQuery::Invocations {
                    account: 2,
                    after: 0,
                    limit: 100,
                }),
            )
            .await
            .unwrap();
        panic!(
            "forge run missing: {}",
            String::from_utf8(invocations).unwrap()
        );
    };
    let bytes = network
        .host
        .query(
            "dispatch",
            &dispatch::encode_query(&dispatch::DispatchQuery::Dispatch {
                receiver: "runs".into(),
                dispatch_id: run.dispatch_id.clone(),
            }),
        )
        .await
        .unwrap();
    let dispatch::DispatchReply::Dispatch(Some(dispatch::DispatchView {
        status: dispatch::DispatchStatus::AwaitingResult { saga_id },
        ..
    })) = dispatch::decode_reply(&bytes).unwrap()
    else {
        panic!("dispatch");
    };
    network
        .submit(
            provider(),
            msg(
                "saga",
                &saga::SagaMsg::Accept {
                    saga_id: saga_id.clone(),
                    attempt: 0,
                },
            ),
        )
        .await;
    assert_eq!(run.run_id, runs::run_id_for("forge:demo:1", 1, "builder"));
    assert!(run.run_id.contains('\u{1f}'));
    network
        .submit(
            provider(),
            msg(
                "runs",
                &runs::RunsMsg::OpenAgentSession {
                    attempt: 0,
                    run_id: run.run_id.clone(),
                    session_key: vec![9; 32],
                },
            ),
        )
        .await;
    network
        .submit(
            session(),
            msg(
                "runs",
                &runs::RunsMsg::AgentAction {
                    run_id: run.run_id.clone(),
                    request_id: "progress".into(),
                    action: reply("Working on this issue"),
                },
            ),
        )
        .await;
    network.drain().await;
    assert!(matches!(
        network
            .action(&runs::action_request_id(&run.run_id, "progress"))
            .await
            .status,
        runs::ActionStatus::Completed {
            outcome: dispatch::CallOutcomeSummary::Applied { .. },
            ..
        }
    ));
    let bytes = network
        .host
        .query(
            "chat",
            &chat::encode_query(&chat::ChatQuery::Message {
                message_id: runs::post_message_id(&run.run_id, "s0"),
            }),
        )
        .await
        .unwrap();
    let chat::ChatReply::Message(Some(progress)) = chat::decode_reply(&bytes).unwrap() else {
        panic!("the program must commit the live reply before the provider finishes");
    };
    assert_eq!(progress.head.author, chat::Party::Account(2));
    assert_eq!(progress.head.origin, sdk::Origin::Program(2));
    assert_eq!(progress.head.thread, Some(1));
    assert_eq!(
        progress.head.blocks,
        vec![chat::Block::paragraph("Working on this issue")]
    );
    let result = sdk::wire::encode(&serde_json::json!({
        "ducktape_runner_result": 1,
        "response_text": serde_json::json!({
            "reply_blocks": [{"id":"reply", "kind":"paragraph", "text":"Implemented the requested change."}],
            "actions": actions,
        }).to_string(),
        "sink": {"mode":"pr", "repo":"demo", "source_branch":"agent/item-1", "target_branch":"dev"},
        "workspace_receipt": {"source_prefix":"forge:demo", "source_snapshot":null, "output_snapshot":null, "commit_height":null, "rebased":false, "no_changes":false, "branch":"agent/item-1", "output_commit":"1a".repeat(20)}
    }));
    network
        .submit(
            provider(),
            msg(
                "saga",
                &saga::SagaMsg::OracleResult {
                    saga_id,
                    attempt: 0,
                    outcome: Ok(result),
                    usage: None,
                },
            ),
        )
        .await;
    while history(&network, &run.run_id).await.is_none() {
        assert!(
            network.host.has_pending_work().await.unwrap(),
            "result delivery remains queued"
        );
        network.step().await;
    }
    assert_eq!(
        history(&network, &run.run_id)
            .await
            .unwrap()
            .pr
            .map(|pr| pr.number),
        None
    );
    assert!(
        item(&network, 2).await.is_none(),
        "a queued proposal has not opened the predicted PR"
    );
    (network, run)
}

#[test]
fn history_links_the_actual_program_allocation_after_another_item_wins_the_next_number() {
    block_on(async {
        let directory = Directory::new("allocated");
        let (mut network, run) = awaiting_pr(&directory, runs::model_program("builder")).await;
        issue(&mut network, "Another transaction allocates item two").await;
        network.drain().await;
        assert_eq!(
            history(&network, &run.run_id).await.unwrap().outcome,
            runs::RunOutcome::ResultAccepted
        );
        let bytes = network
            .host
            .query(
                "chat",
                &chat::encode_query(&chat::ChatQuery::Message {
                    message_id: runs::reply_message_id(&run.run_id),
                }),
            )
            .await
            .unwrap();
        let chat::ChatReply::Message(Some(reply)) = chat::decode_reply(&bytes).unwrap() else {
            panic!("explicit RequestRun reply must actually commit");
        };
        assert_eq!(reply.head.author, chat::Party::Account(2));
        assert_eq!(reply.head.origin, sdk::Origin::Program(2));
        assert_eq!(reply.head.thread, Some(1));
        assert_eq!(
            history(&network, &run.run_id)
                .await
                .unwrap()
                .pr
                .map(|pr| pr.number),
            Some(3)
        );
        let opened = item(&network, 3).await.unwrap();
        assert_eq!(opened.summary.author, chat::Party::Account(2));
        assert_eq!(opened.source_branch.as_deref(), Some("agent/item-1"));
        assert_eq!(opened.target_branch.as_deref(), Some("dev"));
    });
}

async fn next_update(network: &Network) -> Option<runs::ModuleUpdateView> {
    let bytes = network
        .host
        .query(
            "runs",
            &runs::encode_query(&runs::RunsQuery::NextModuleUpdate),
        )
        .await
        .unwrap();
    let runs::RunsReply::ModuleUpdate(update) = runs::decode_reply(&bytes).unwrap() else {
        panic!("module update reply");
    };
    update
}

fn replacement_action() -> runs::ActionEnvelope {
    update_module(runs::ModuleUpdateSpec {
        module_id: "hello".into(),
        artifact: "hello.module".into(),
        code_hash: "ab".repeat(32),
        after: 50,
    })
}

#[test]
fn a_deployment_waits_for_the_program_and_pins_the_host_pushed_commit() {
    block_on(async {
        let directory = Directory::new("deployment");
        let (mut network, run) = awaiting_pr_with_actions(
            &directory,
            runs::model_program("builder"),
            vec![replacement_action()],
        )
        .await;
        assert!(
            next_update(&network).await.is_none(),
            "model output alone cannot deploy"
        );
        network.drain().await;
        let update = next_update(&network)
            .await
            .expect("program queued deployment");
        assert_eq!(update.request.account, 2);
        assert_eq!(update.request.run_id, run.run_id);
        assert_eq!(update.request.source.repo, "demo");
        assert_eq!(update.request.source.branch, "agent/item-1");
        assert_eq!(update.request.source.commit, "1a".repeat(20));
        assert_eq!(update.status, runs::ModuleUpdateStatus::Requested);
        let receipt = network
            .action(&format!("result/{}/0", run.dispatch_id))
            .await;
        assert!(
            matches!(
                receipt.status,
                runs::ActionStatus::Completed {
                    outcome: dispatch::CallOutcomeSummary::Applied { .. },
                    ..
                }
            ),
            "{receipt:?}"
        );
        network.drain().await;
        assert_eq!(
            next_update(&network).await,
            Some(update),
            "draining twice cannot enqueue twice"
        );
    });
}

#[test]
fn a_program_without_a_deployment_route_queues_no_upgrade() {
    block_on(async {
        let directory = Directory::new("deployment-refused");
        let mut program = runs::model_program("builder");
        for step in &mut program.steps {
            let agent::Step::Call { module, msg, .. } = step else {
                continue;
            };
            let deployment_target = module == "runs" && matches!(msg, agent::Value::Ref(_));
            if deployment_target {
                *step = agent::Step::Finish;
            }
        }
        let (mut network, _) =
            awaiting_pr_with_actions(&directory, program, vec![replacement_action()]).await;
        network.drain().await;
        assert!(next_update(&network).await.is_none());
    });
}

#[test]
fn a_rejected_program_target_never_links_a_predicted_pr() {
    block_on(async {
        let directory = Directory::new("rejected");
        let (mut network, run) = awaiting_pr(&directory, runs::model_program("builder")).await;
        network
            .submit(
                member(),
                msg("forge", &push("agent/item-1", Some(0x1a), None)),
            )
            .await;
        network.drain().await;
        assert_eq!(
            history(&network, &run.run_id)
                .await
                .unwrap()
                .pr
                .map(|pr| pr.number),
            None
        );
        assert!(item(&network, 2).await.is_none());
        assert_eq!(
            history(&network, &run.run_id).await.unwrap().outcome,
            runs::RunOutcome::ActionRejected
        );
        let receipt = network
            .action(&format!("result/{}/1", run.dispatch_id))
            .await;
        assert!(
            matches!(
                receipt.status,
                runs::ActionStatus::Completed {
                    outcome: dispatch::CallOutcomeSummary::Rejected { .. },
                    ..
                }
            ),
            "{receipt:?}"
        );
    });
}

#[test]
fn a_program_that_omits_the_target_leaves_the_pr_link_empty() {
    block_on(async {
        let directory = Directory::new("omitted");
        let mut program = runs::model_program("builder");
        for step in &mut program.steps {
            if matches!(step, agent::Step::Call { module, .. } if module == "forge") {
                *step = agent::Step::Finish;
            }
        }
        let (mut network, run) = awaiting_pr(&directory, program).await;
        network.drain().await;
        assert_eq!(
            history(&network, &run.run_id)
                .await
                .unwrap()
                .pr
                .map(|pr| pr.number),
            None
        );
        assert!(item(&network, 2).await.is_none());
    });
}

#[test]
fn forged_program_output_cannot_redirect_the_link_of_a_successful_call() {
    block_on(async {
        let directory = Directory::new("forged");
        let mut program = runs::model_program("builder");
        for step in &mut program.steps {
            let agent::Step::Call {
                msg: agent::Value::Map(message),
                ..
            } = step
            else {
                continue;
            };
            let Some(agent::Value::Map(completion)) = message.get_mut("complete_action_request")
            else {
                continue;
            };
            completion.insert(
                "result".into(),
                agent::Value::Map(std::collections::BTreeMap::from([(
                    "applied".into(),
                    agent::Value::Map(std::collections::BTreeMap::from([
                        (
                            "output".into(),
                            agent::Value::Map(std::collections::BTreeMap::from([
                                ("repo".into(), agent::Value::Text("demo".into())),
                                ("number".into(), agent::Value::Number(999)),
                            ])),
                        ),
                        ("assigned".into(), agent::Value::Null),
                    ])),
                )])),
            );
        }
        let (mut network, run) = awaiting_pr(&directory, program).await;
        network.drain().await;
        assert_eq!(
            history(&network, &run.run_id)
                .await
                .unwrap()
                .pr
                .map(|pr| pr.number),
            None
        );
        assert_eq!(
            item(&network, 2).await.unwrap().summary.author,
            chat::Party::Account(2)
        );
        let bytes = network
            .host
            .query(
                "agent",
                &agent::encode_query(&agent::AgentQuery::Invocations {
                    account: 2,
                    after: 0,
                    limit: 100,
                }),
            )
            .await
            .unwrap();
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .contains("reported PR output does not match the committed action")
        );
    });
}

async fn deployment_network(label: &str, actions: usize) -> (Directory, Network) {
    let directory = Directory::new(label);
    let (mut network, _) = awaiting_pr_with_actions(
        &directory,
        runs::model_program("builder"),
        (0..actions).map(|_| replacement_action()).collect(),
    )
    .await;
    let mut registry = modules::Modules::new("modules", store(), "valset", "governance");
    registry
        .seed("hello", modules::Kind::Module, vec![0; 32], Vec::new())
        .await
        .unwrap();
    registry.finish_seed().await.unwrap();
    network.host.register(Box::new(registry));
    network.host.register(Box::new(
        governance::Governance::new("governance", store(), "valset", "identity")
            .with_code_registry("modules"),
    ));
    network.drain().await;
    (directory, network)
}

#[test]
fn only_a_validator_can_refuse_unproposed_work_and_retries_do_not_enqueue_it_again() {
    block_on(async {
        let (_directory, mut network) = deployment_network("deployment-refusal", 2).await;
        let first = next_update(&network).await.unwrap();
        let refusal = runs::RunsMsg::RefuseModuleUpdate {
            sequence: 0,
            reason: "artifact hash mismatch".into(),
        };
        network.height += 1;
        let denied = network
            .host
            .submit_at(
                host::BlockContext {
                    height: network.height,
                    consensus_time: network.height,
                    origin: member(),
                },
                msg("runs", &refusal),
            )
            .await;
        assert!(denied.is_err());
        assert_eq!(next_update(&network).await, Some(first.clone()));
        network.submit(provider(), msg("runs", &refusal)).await;
        assert_eq!(next_update(&network).await.unwrap().request.sequence, 1);
        let request = first.request;
        network
            .submit(
                sdk::Origin::Program(request.account),
                msg(
                    "runs",
                    &runs::RunsMsg::RequestModuleUpdate {
                        request_id: request.request_id,
                        run_id: request.run_id,
                        source: request.source,
                        update: request.update,
                    },
                ),
            )
            .await;
        network
            .submit(
                provider(),
                msg(
                    "runs",
                    &runs::RunsMsg::RefuseModuleUpdate {
                        sequence: 1,
                        reason: "artifact hash mismatch".into(),
                    },
                ),
            )
            .await;
        assert!(
            next_update(&network).await.is_none(),
            "retry must not become sequence two"
        );
        let bytes = network
            .host
            .query(
                "runs",
                &runs::encode_query(&runs::RunsQuery::ModuleUpdate { sequence: 0 }),
            )
            .await
            .unwrap();
        let runs::RunsReply::ModuleUpdate(Some(view)) = runs::decode_reply(&bytes).unwrap() else {
            panic!("durable rejected request")
        };
        assert!(matches!(
            view.status,
            runs::ModuleUpdateStatus::Rejected { .. }
        ));
    });
}

#[test]
fn reconciliation_requires_registry_evidence_and_expired_readiness_releases_the_queue() {
    block_on(async {
        let (_directory, mut network) = deployment_network("deployment-expired", 1).await;
        let first = next_update(&network).await.unwrap();
        let reconcile = runs::RunsMsg::ReconcileModuleUpdate { sequence: 0 };
        network.submit(member(), msg("runs", &reconcile)).await;
        assert_eq!(
            next_update(&network).await,
            Some(first.clone()),
            "a caller cannot claim activation"
        );
        let id = runs::module_update_proposal_id(0);
        network
            .submit(
                provider(),
                msg(
                    "governance",
                    &governance::GovMsg::Propose {
                        proposal_id: id.clone(),
                        voting_period: 1_000,
                        action: governance::GovAction::UpdateModule {
                            name: id.clone(),
                            module_id: "hello".into(),
                            activation_lead: first.request.update.after,
                            code_hash: first.request.update.digest().unwrap().to_vec(),
                        },
                    },
                ),
            )
            .await;
        for voter in [7, 8] {
            network
                .submit(
                    sdk::Origin::External(vec![voter; 32]),
                    msg(
                        "governance",
                        &governance::GovMsg::Vote {
                            proposal_id: id.clone(),
                            approve: true,
                        },
                    ),
                )
                .await;
        }
        network
            .submit(
                provider(),
                msg(
                    "governance",
                    &governance::GovMsg::Execute { proposal_id: id },
                ),
            )
            .await;
        network.drain().await;
        network.height += 1;
        let denied = network
            .host
            .submit_at(
                host::BlockContext {
                    height: network.height,
                    consensus_time: network.height,
                    origin: provider(),
                },
                msg(
                    "runs",
                    &runs::RunsMsg::RefuseModuleUpdate {
                        sequence: 0,
                        reason: "too late to refuse".into(),
                    },
                ),
            )
            .await;
        assert!(denied.is_err());
        assert_eq!(
            next_update(&network).await,
            Some(first.clone()),
            "a validator cannot override a committed proposal"
        );
        // Advancing these exact blocks exercises the activation lead, not a wall-clock wait.
        for _ in 0..first.request.update.after {
            network.step().await;
        }
        network.submit(member(), msg("runs", &reconcile)).await;
        assert!(next_update(&network).await.is_none());
        let bytes = network
            .host
            .query(
                "runs",
                &runs::encode_query(&runs::RunsQuery::ModuleUpdate { sequence: 0 }),
            )
            .await
            .unwrap();
        let runs::RunsReply::ModuleUpdate(Some(view)) = runs::decode_reply(&bytes).unwrap() else {
            panic!("durable expired request")
        };
        assert_eq!(
            view.status,
            runs::ModuleUpdateStatus::Rejected {
                reason: "deployment expired before every validator was ready".into()
            }
        );
    });
}

async fn work(network: &Network, voter: u8) -> Option<node_work::Directive> {
    let query = node_work::Query::NodeWork {
        node_key: vec![voter; 32],
        height: network.height,
        consensus_time: network.height,
    };
    let bytes = network
        .host
        .query("runs", &sdk::wire::encode(&query))
        .await
        .unwrap();
    let node_work::Reply::NodeWork(work) = sdk::wire::decode(&bytes).unwrap();
    work
}

async fn apply_work(network: &mut Network, voter: u8, message: node_work::Submission) {
    network
        .submit(
            sdk::Origin::External(vec![voter; 32]),
            sdk::Msg {
                target: message.target,
                payload: message.payload,
            },
        )
        .await;
}

#[test]
fn the_module_projects_the_entire_ceremony_and_retries_from_committed_receipts() {
    block_on(async {
        let (_directory, mut network) = deployment_network("work-projection", 1).await;
        assert!(
            work(&network, 1).await.is_none(),
            "non-members receive no node work"
        );
        let node_work::Directive::StageBlob { blob, on_ready, .. } =
            work(&network, 7).await.unwrap()
        else {
            panic!("stage first")
        };
        assert_eq!(blob.commit, "1a".repeat(20));
        assert_eq!(blob.hash, [0xab; 32]);
        assert_eq!(blob.path, "hello.module");
        apply_work(&mut network, 7, on_ready.clone()).await;
        apply_work(&mut network, 7, on_ready).await;
        let node_work::Directive::Submit(propose) = work(&network, 7).await.unwrap() else {
            panic!("propose")
        };
        assert!(matches!(
            governance::decode_msg(&propose.payload).unwrap(),
            governance::GovMsg::Propose { .. }
        ));
        apply_work(&mut network, 7, propose).await;
        let node_work::Directive::Submit(vote) = work(&network, 7).await.unwrap() else {
            panic!("first vote")
        };
        apply_work(&mut network, 7, vote).await;
        assert!(
            work(&network, 7).await.is_none(),
            "a repeated wake does not vote twice"
        );
        let node_work::Directive::StageBlob { on_ready, .. } = work(&network, 8).await.unwrap()
        else {
            panic!("each validator stages before voting")
        };
        apply_work(&mut network, 8, on_ready).await;
        let node_work::Directive::Submit(vote) = work(&network, 8).await.unwrap() else {
            panic!("second vote")
        };
        apply_work(&mut network, 8, vote).await;
        let node_work::Directive::Submit(execute) = work(&network, 7).await.unwrap() else {
            panic!("execute")
        };
        assert!(matches!(
            governance::decode_msg(&execute.payload).unwrap(),
            governance::GovMsg::Execute { .. }
        ));
        apply_work(&mut network, 7, execute).await;
        network.drain().await;
        assert!(
            work(&network, 7).await.is_none(),
            "an in-flight swap owns its activation lead"
        );
        for _ in 0..50 {
            network.step().await;
        }
        let node_work::Directive::Submit(reconcile) = work(&network, 7).await.unwrap() else {
            panic!("reconcile expiry")
        };
        apply_work(&mut network, 7, reconcile.clone()).await;
        apply_work(&mut network, 7, reconcile).await;
        assert!(work(&network, 7).await.is_none());
        assert!(next_update(&network).await.is_none());
    });
}
