//! The shipping initializer on the real Host queue: no timer or boot hook.
#[path = "../src/chief_cli/plan.rs"]
mod plan;
#[path = "../src/wire.rs"]
mod wire;
#[path = "chief_support/mod.rs"]
mod support;

use futures::executor::block_on;
use support::{Network, member, msg};
use wire::{agent, chat, pages, runs};

fn fixture(existing: Option<&str>) -> (plan::Plan, agent::Program, u64) {
    let plan = plan::Plan::new(1, "chief", existing, "worker").unwrap();
    let package = run_envelope::ConversationPackage {
        name: "chief".into(),
        source_prefix: format!("{}/package", plan.root()),
        source_snapshot: "a".repeat(64),
    };
    let (program, offset) = plan::program(&plan, &package);
    (plan, program, offset)
}
async fn query<T: serde::de::DeserializeOwned>(
    network: &Network,
    target: &str,
    query: impl serde::Serialize,
) -> T {
    sdk::wire::decode(
        &network
            .host
            .query(target, &sdk::wire::encode(&query))
            .await
            .unwrap(),
    )
    .unwrap()
}
async fn provision(network: &mut Network, plan: &plan::Plan, program: &agent::Program) {
    network
        .submit(
            member(),
            msg(
                "agent",
                &agent::AgentMsg::Provision {
                    request_id: plan.namespace.clone(),
                    name: "Chief".into(),
                    program: program.clone(),
                },
            ),
        )
        .await;
}
async fn initialize(network: &mut Network, request_id: &str) {
    network
        .submit(
            member(),
            msg(
                "agent",
                &agent::AgentMsg::Initialize {
                    account: 2,
                    request_id: request_id.into(),
                },
            ),
        )
        .await;
    network.drain().await;
}
async fn conversation(network: &Network, plan: &plan::Plan) -> Option<runs::ConversationView> {
    let reply = query(
        network,
        "runs",
        runs::RunsQuery::Conversation {
            conversation_id: plan.namespace.clone(),
        },
    )
    .await;
    let runs::RunsReply::Conversation(conversation) = reply else {
        panic!("conversation reply")
    };
    conversation
}
async fn invocations(network: &Network) -> Vec<agent::InvocationEntry> {
    let reply = query(
        network,
        "agent",
        agent::AgentQuery::Invocations {
            account: 2,
            after: 0,
            limit: 64,
        },
    )
    .await;
    let agent::AgentReply::Invocations(entries) = reply else {
        panic!("invocations reply")
    };
    entries
}
/// The bed this suite runs on. The committed `runs` guest compiles its whole
/// sibling wiring in — chat, saga, attribution, dispatch, agent, tasks, files,
/// forge, pages — so there is nothing here to re-register on top of the genesis
/// [`Network`] composes.
async fn network() -> Network {
    let mut network = Network::new().await;
    network
        .submit(
            member(),
            msg(
                "identity",
                &identity::IdentityMsg::Create {
                    name: "Alice".into(),
                    scheme: identity::KeyScheme::Ed25519,
                },
            ),
        )
        .await;
    network
}

async fn host_fixture(
    network: &mut Network,
    existing: Option<&str>,
) -> (plan::Plan, agent::Program, u64) {
    use base64::Engine as _;
    let plan = plan::Plan::new(1, "chief", existing, "worker").unwrap();
    let prefix = format!("{}/package", plan.root());
    let files::FilesReply::Refs(refs) = query(network, "files", files::FilesQuery::Refs {}).await
    else {
        panic!("Files refs")
    };
    let mut changes: Vec<_> = [
        (
            "package.json",
            r#"{"name":"chief","pi":{"extensions":["index.js"]}}"#,
        ),
        ("index.js", "export default () => {};\n"),
    ]
    .into_iter()
    .map(|(name, body)| files::Change::Put {
        path: format!("{prefix}/{name}"),
        exec: false,
        meta: Default::default(),
        content: files::Content::Inline {
            b64: base64::engine::general_purpose::STANDARD.encode(body),
        },
    })
    .collect();
    changes.push(files::Change::Put {
        path: "/shared/unrelated.txt".into(),
        exec: false,
        meta: Default::default(),
        content: files::Content::Inline {
            b64: base64::engine::general_purpose::STANDARD.encode("not package content"),
        },
    });
    network
        .submit(
            member(),
            msg(
                "files",
                &files::FilesMsg::Commit {
                    base_snapshot: refs.head,
                    message: "generic resident fixture".into(),
                    changes,
                },
            ),
        )
        .await;
    let files::FilesReply::Refs(refs) = query(network, "files", files::FilesQuery::Refs {}).await
    else {
        panic!("Files refs")
    };
    let candidate = refs.head.clone().unwrap();
    network.height += 1;
    let outcome = network
        .host
        .submit_at(
            host::BlockContext {
                height: network.height,
                consensus_time: network.height,
                origin: member(),
            },
            msg(
                "files",
                &files::FilesMsg::ProjectSnapshot {
                    snapshot: candidate.clone(),
                    path: prefix.clone(),
                },
            ),
        )
        .await
        .unwrap();
    let projected = &outcome.dispatches[0];
    assert_eq!(projected.module, "files");
    assert_eq!(projected.origin, member());
    assert_eq!(projected.output.as_ref().unwrap(), &projected.assigned);
    let output = files::decode_write_output(&projected.assigned).unwrap();
    let files::WriteOutcome::ProjectSnapshot { snapshot } = output.outcome else {
        panic!("projection receipt")
    };
    network.events.extend(outcome.events);
    let files::FilesReply::Refs(after) = query(network, "files", files::FilesQuery::Refs {}).await
    else {
        panic!("refs")
    };
    assert_eq!(after.head, refs.head, "projection never changes HEAD");
    let unrelated: files::FilesReply = query(
        network,
        "files",
        files::FilesQuery::Stat {
            path: "/shared/unrelated.txt".into(),
            snapshot: Some(snapshot.clone()),
        },
    )
    .await;
    assert_eq!(unrelated, files::FilesReply::Stat(None));
    let original: files::FilesReply = query(
        network,
        "files",
        files::FilesQuery::Stat {
            path: "/shared/unrelated.txt".into(),
            snapshot: Some(candidate),
        },
    )
    .await;
    assert!(matches!(original, files::FilesReply::Stat(Some(_))));
    for file in ["package.json", "index.js"] {
        let present: files::FilesReply = query(
            network,
            "files",
            files::FilesQuery::Stat {
                path: format!("{prefix}/{file}"),
                snapshot: Some(snapshot.clone()),
            },
        )
        .await;
        assert!(
            matches!(present, files::FilesReply::Stat(Some(_))),
            "original package path retained"
        );
    }
    let package = run_envelope::ConversationPackage {
        name: "chief".into(),
        source_prefix: prefix,
        source_snapshot: snapshot,
    };
    let (program, offset) = plan::program(&plan, &package);
    (plan, program, offset)
}

#[test]
fn package_projection_receipt_excludes_unrelated_tree_and_does_not_move_head() {
    block_on(async {
        let mut network = network().await;
        // The fixture asserts original package paths, no unrelated file in the
        // exact assigned snapshot, identical assigned/output and unchanged HEAD.
        host_fixture(&mut network, None).await;
    });
}

#[test]
fn transferred_chief_keeps_installation_namespace_and_only_new_holder_controls_it() {
    block_on(async {
        let mut network = network().await;
        let (plan, program, _) = host_fixture(&mut network, None).await;
        provision(&mut network, &plan, &program).await;
        network.drain().await;
        initialize(&mut network, "install").await;
        let new_holder = sdk::Origin::External(vec![8; 32]);
        network
            .submit(
                new_holder.clone(),
                msg(
                    "identity",
                    &identity::IdentityMsg::Create {
                        name: "new holder".into(),
                        scheme: identity::KeyScheme::Ed25519,
                    },
                ),
            )
            .await;
        let identity::IdentityReply::Account(Some(holder)) = query(
            &network,
            "identity",
            identity::IdentityQuery::OfKey { key: vec![8; 32] },
        )
        .await
        else {
            panic!("new account")
        };
        network
            .submit(
                member(),
                msg(
                    "identity",
                    &identity::IdentityMsg::TransferControl {
                        account: 2,
                        to: holder.number,
                    },
                ),
            )
            .await;
        for (action, active) in [(plan::Control::Pause, false), (plan::Control::Resume, true)] {
            let before = conversation(&network, &plan).await.unwrap();
            let messages =
                plan::control_messages(&before, action, &format!("transferred-{active}")).unwrap();
            for message in messages {
                network.height += 1;
                let rejected = network
                    .host
                    .submit_at(
                        host::BlockContext {
                            height: network.height,
                            consensus_time: network.height,
                            origin: member(),
                        },
                        msg("runs", &message),
                    )
                    .await;
                assert!(
                    rejected.is_err(),
                    "historical installer cannot control the transferred Chief"
                );
                network
                    .submit(new_holder.clone(), msg("runs", &message))
                    .await;
            }
            let after = conversation(&network, &plan).await.unwrap();
            assert_eq!(
                after.conversation_id, plan.namespace,
                "address remains the original installation"
            );
            assert_eq!(
                after.status,
                if active {
                    runs::ConversationStatus::Active
                } else {
                    runs::ConversationStatus::Inactive
                }
            );
        }
    });
}

#[test]
fn stable_plan_is_controller_scoped_and_config_contains_only_public_bindings() {
    let (plan, program, _) = fixture(None);
    assert_eq!(plan, plan::Plan::new(1, "chief", None, "worker").unwrap());
    assert_ne!(
        plan.namespace,
        plan::Plan::new(2, "chief", None, "worker")
            .unwrap()
            .namespace
    );
    assert_eq!(plan.config().as_object().unwrap().len(), 6);
    assert!(plan.namespace.len() <= agent::MAX_PROVISION_REQUEST_ID_BYTES);
    assert_ne!(plan.home_page_id, plan.board_page_id);
    assert_ne!(plan.board_page_id, plan.inbox_page_id);
    assert!(plan::Plan::new(1, "../escape", None, "worker").is_err());
    let decoded: agent::Program =
        serde_json::from_slice(&serde_json::to_vec(&program).unwrap()).unwrap();
    assert_eq!(program, decoded);
}

#[test]
fn relocated_program_keeps_forward_targets_and_action_call_positions() {
    let (_, program, offset) = fixture(None);
    for (index, step) in program.steps.iter().enumerate() {
        let validate = |target: u64| {
            assert!(target > index as u64);
            assert!(target <= program.steps.len() as u64);
        };
        match step {
            agent::Step::Branch { then, or, .. } => {
                validate(*then);
                validate(*or);
            }
            agent::Step::Call {
                msg, on_failure, ..
            } => {
                if let agent::Continuation::Step(target) = on_failure {
                    validate(*target);
                }
                let agent::Value::Map(operation) = msg else {
                    continue;
                };
                let Some(agent::Value::Map(fields)) = operation.get("claim_action_request") else {
                    continue;
                };
                let agent::Value::Number(target) = fields["target_step"] else {
                    panic!("absolute call step")
                };
                assert_eq!(target, index as i128 + 1);
                assert!(target >= i128::from(offset));
                assert!(matches!(
                    program.steps[target as usize],
                    agent::Step::Call { .. }
                ));
                let agent::Step::Call {
                    msg: agent::Value::Map(complete),
                    ..
                } = &program.steps[target as usize + 1]
                else {
                    panic!("completion step")
                };
                let agent::Value::Map(fields) = &complete["complete_action_request"] else {
                    panic!("fields")
                };
                let agent::Value::Map(call) = &fields["call"] else {
                    panic!("call")
                };
                assert_eq!(call["step"], agent::Value::Number(target));
            }
            agent::Step::Dispatch { on_failure, .. } => {
                if let agent::Continuation::Step(target) = on_failure {
                    validate(*target);
                }
            }
            agent::Step::Query { .. } | agent::Step::Report { .. } | agent::Step::Finish => {}
        }
    }
}

#[test]
fn explicit_initialization_is_idempotent_and_does_not_reset_renames_or_pause() {
    block_on(async {
        let mut network = network().await;
        let (plan, program, _) = host_fixture(&mut network, None).await;
        provision(&mut network, &plan, &program).await;
        network.drain().await;
        assert!(
            conversation(&network, &plan).await.is_none(),
            "provision/draining is not logical initialization"
        );
        let reply: pages::PageReply = query(
            &network,
            "pages",
            pages::PageQuery::GetBlock {
                block_id: plan.home_page_id.clone(),
            },
        )
        .await;
        assert_eq!(
            reply,
            pages::PageReply::Block(None),
            "no boot-created Pages"
        );
        initialize(&mut network, "install").await;
        let entries = invocations(&network).await;
        assert!(
            entries
                .iter()
                .all(|entry| !matches!(entry.invocation.status, agent::Status::Failed { .. })),
            "{entries:#?}"
        );
        let state = conversation(&network, &plan)
            .await
            .expect("initialized conversation");
        assert_eq!(state.account, 2);
        assert_eq!(state.status, runs::ConversationStatus::Active);
        for (page_id, _) in plan.pages() {
            let reply: pages::PageReply = query(
                &network,
                "pages",
                pages::PageQuery::RecordCollection {
                    page_id: page_id.into(),
                },
            )
            .await;
            let pages::PageReply::RecordCollection(Some(collection)) = reply else {
                panic!("owned collection")
            };
            assert_eq!(collection.writer, pages::Party::Account(2));
            assert_eq!((collection.revision, collection.record_count), (0, 0));
            let guidance: pages::PageReply = query(
                &network,
                "pages",
                pages::PageQuery::GetBlock {
                    block_id: format!("{page_id}-guide"),
                },
            )
            .await;
            let pages::PageReply::Block(Some(guidance)) = guidance else {
                panic!("workspace guidance")
            };
            assert!(guidance.text.contains("fresh comment"));
            assert!(guidance.text.contains("shared Chat channel"));
        }
        let count = entries.len();
        provision(&mut network, &plan, &program).await;
        initialize(&mut network, "install").await;
        assert_eq!(
            invocations(&network).await.len(),
            count,
            "same initialization receipt emits nothing new"
        );
        assert_eq!(conversation(&network, &plan).await.unwrap(), state);
        network
            .submit(
                member(),
                msg(
                    "chat",
                    &chat::ChatMsg::RenameChannel {
                        channel_id: plan.channel_id.clone(),
                        name: "Coordination".into(),
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                msg(
                    "runs",
                    &runs::RunsMsg::ActivateConversation {
                        conversation_id: plan.namespace.clone(),
                        operation_id: "human-pause".into(),
                        active: false,
                    },
                ),
            )
            .await;
        network.drain().await;
        let paused = conversation(&network, &plan).await.unwrap();
        assert_eq!(paused.status, runs::ConversationStatus::Inactive);
        // Even a newly authorized initialization cannot undo a later pause:
        // its fixed activation receipt and existing conversation bind once.
        initialize(&mut network, "explicit-reconciliation").await;
        assert_eq!(conversation(&network, &plan).await.unwrap(), paused);
        let reply: chat::ChatReply = query(
            &network,
            "chat",
            chat::ChatQuery::Channel {
                channel_id: plan.channel_id.clone(),
            },
        )
        .await;
        let chat::ChatReply::Channel(Some(channel)) = reply else {
            panic!("channel")
        };
        assert_eq!(channel.name, "Coordination");
        let reply: pages::PageReply = query(
            &network,
            "pages",
            pages::PageQuery::GetBlock {
                block_id: plan.home_page_id.clone(),
            },
        )
        .await;
        let pages::PageReply::Block(Some(page)) = reply else {
            panic!("page")
        };
        assert_eq!(page.text, "Chief");
        network
            .submit(
                member(),
                msg(
                    "runs",
                    &runs::RunsMsg::ActivateConversation {
                        conversation_id: plan.namespace.clone(),
                        operation_id: "human-resume".into(),
                        active: true,
                    },
                ),
            )
            .await;
        network.drain().await;
        let resumed = conversation(&network, &plan).await.unwrap();
        assert_eq!(resumed.status, runs::ConversationStatus::Active);
        assert_eq!(resumed.source_cursor, paused.source_cursor);
        assert_eq!(resumed.history, paused.history);
        assert_eq!(resumed.packages, paused.packages);
    });
}

#[test]
fn explicit_existing_channel_starts_at_current_cursor_without_replaying_history() {
    block_on(async {
        let mut network = network().await;
        network
            .submit(
                member(),
                msg(
                    "chat",
                    &chat::ChatMsg::CreateChannel {
                        channel_id: "existing".into(),
                        name: "Existing".into(),
                        post_policy: chat::PostPolicy::Open,
                    },
                ),
            )
            .await;
        network
            .submit(
                member(),
                msg(
                    "chat",
                    &chat::ChatMsg::PostMessage {
                        channel_id: "existing".into(),
                        message_id: "old".into(),
                        thread: None,
                        blocks: vec![chat::Block::Paragraph(vec![chat::Span {
                            text: "old instruction is not fresh authority".into(),
                            marks: vec![],
                        }])],
                    },
                ),
            )
            .await;
        let (plan, program, _) = host_fixture(&mut network, Some("existing")).await;
        provision(&mut network, &plan, &program).await;
        initialize(&mut network, "install").await;
        let state = conversation(&network, &plan)
            .await
            .expect("bound existing conversation");
        assert_eq!(state.source_cursor, 1);
        assert_eq!(state.admitted_cursor, 0);
        assert!(state.active_turn.is_none());
        initialize(&mut network, "install").await;
        assert_eq!(conversation(&network, &plan).await.unwrap(), state);
    });
}

#[test]
fn resume_recovers_an_interrupted_native_turn_from_its_committed_checkpoint() {
    use base64::Engine as _;
    block_on(async {
        let mut network = network().await;
        network
            .submit(
                support::provider(),
                msg(
                    "capability",
                    &capability::CapabilityMsg::Announce {
                        capabilities: vec!["pi".into()],
                        resources: Default::default(),
                    },
                ),
            )
            .await;
        let (plan, program, _) = host_fixture(&mut network, None).await;
        provision(&mut network, &plan, &program).await;
        initialize(&mut network, "install").await;
        network
            .submit(
                member(),
                msg(
                    "chat",
                    &chat::ChatMsg::PostMessage {
                        channel_id: plan.channel_id.clone(),
                        message_id: "coordinate".into(),
                        thread: None,
                        blocks: vec![chat::Block::Paragraph(vec![chat::Span {
                            text: "coordinate this work".into(),
                            marks: vec![],
                        }])],
                    },
                ),
            )
            .await;
        network.drain().await;
        let initialized = conversation(&network, &plan).await;
        assert!(
            initialized.is_some(),
            "{:?}",
            invocations(&network)
                .await
                .iter()
                .map(|entry| &entry.invocation.status)
                .collect::<Vec<_>>()
        );
        let running = initialized.unwrap();
        let turn = running.active_turn.as_ref().expect("coordinating turn");
        assert_eq!(turn.phase, runs::ConversationTurnPhase::Running);
        let run_id = turn.run_id.clone();
        let reply: dispatch::DispatchReply = query(
            &network,
            "dispatch",
            dispatch::DispatchQuery::Dispatch {
                receiver: "runs".into(),
                dispatch_id: runs::dispatch_id_for(&run_id),
            },
        )
        .await;
        let dispatch::DispatchReply::Dispatch(Some(dispatch::DispatchView {
            status: dispatch::DispatchStatus::AwaitingResult { saga_id },
            ..
        })) = reply
        else {
            panic!("native run saga")
        };
        network
            .submit(
                support::provider(),
                msg(
                    "saga",
                    &saga::SagaMsg::Accept {
                        saga_id: saga_id.clone(),
                        attempt: 0,
                    },
                ),
            )
            .await;
        network
            .submit(
                support::provider(),
                msg(
                    "runs",
                    &runs::RunsMsg::OpenAgentSession {
                        run_id: run_id.clone(),
                        attempt: 0,
                        session_key: vec![9; 32],
                    },
                ),
            )
            .await;
        for command in
            plan::control_messages(&running, plan::Control::Pause, "human-pause-1").unwrap()
        {
            network.submit(member(), msg("runs", &command)).await;
        }
        network
            .submit(
                member(),
                msg(
                    "files",
                    &files::FilesMsg::Commit {
                        base_snapshot: None,
                        message: "native checkpoint".into(),
                        changes: vec![files::Change::Put {
                            path: format!("{}/{}", running.history_prefix, running.session_path),
                            exec: false,
                            meta: Default::default(),
                            content: files::Content::Inline {
                                b64: base64::engine::general_purpose::STANDARD
                                    .encode(b"opaque committed native checkpoint\n"),
                            },
                        }],
                    },
                ),
            )
            .await;
        let files::FilesReply::Refs(refs) =
            query(&network, "files", files::FilesQuery::Refs {}).await
        else {
            panic!("Files head")
        };
        let history = runs::ConversationHistory {
            revision: 1,
            snapshot: refs.head.unwrap(),
        };
        network
            .submit(
                support::session(),
                msg(
                    "runs",
                    &runs::RunsMsg::CheckpointConversation {
                        conversation_id: plan.namespace.clone(),
                        run_id: run_id.clone(),
                        attempt: 0,
                        operation_id: "native-paused-checkpoint".into(),
                        history: history.clone(),
                        delivery: false,
                    },
                ),
            )
            .await;
        network.drain().await;
        // the attempt budget comes off the saga the module actually registered,
        // never a constant this suite carries: `runs` sets it on the recipe, and
        // reading it back is the only place it is true.
        let saga::SagaReply::Saga(Some(registered)) = query(
            &network,
            "saga",
            saga::SagaQuery::Get {
                saga_id: saga_id.clone(),
            },
        )
        .await
        else {
            panic!("saga view")
        };
        for attempt in 0..registered.max_attempts {
            if attempt > 0 {
                network
                    .submit(
                        support::provider(),
                        msg(
                            "saga",
                            &saga::SagaMsg::Accept {
                                saga_id: saga_id.clone(),
                                attempt,
                            },
                        ),
                    )
                    .await;
            }
            network
                .submit(
                    support::provider(),
                    msg(
                        "saga",
                        &saga::SagaMsg::OracleResult {
                            saga_id: saga_id.clone(),
                            attempt,
                            outcome: Err("native execution interrupted".into()),
                            usage: None,
                        },
                    ),
                )
                .await;
            network.drain().await;
        }
        let failed = conversation(&network, &plan).await.unwrap();
        assert!(
            matches!(failed.status, runs::ConversationStatus::Paused { .. }),
            "{failed:#?}"
        );
        let previous = failed.active_turn.as_ref().unwrap();
        assert_eq!(previous.phase, runs::ConversationTurnPhase::Draining);
        assert_eq!(previous.outcome, Some(runs::RunOutcome::Failed));
        let mut stopped_reaction = failed.clone();
        stopped_reaction.active_turn.as_mut().unwrap().phase =
            runs::ConversationTurnPhase::AwaitingProgram;
        assert!(
            plan::control_messages(&stopped_reaction, plan::Control::Resume, "no-native-turn")
                .is_err()
        );
        let mut pending_effects = failed.clone();
        pending_effects
            .active_turn
            .as_mut()
            .unwrap()
            .actions
            .push("unsettled-action".into());
        assert!(
            plan::control_messages(&pending_effects, plan::Control::Resume, "too-early").is_err()
        );
        let commands =
            plan::control_messages(&failed, plan::Control::Resume, "human-resume-1").unwrap();
        assert!(matches!(
            commands[0],
            runs::RunsMsg::RetryConversationTurn { .. }
        ));
        // Repeating the exact operation sequence also exercises committed Runs
        // receipts, independently of repeated CLI verbs choosing fresh IDs.
        for _ in 0..2 {
            for command in &commands {
                network.submit(member(), msg("runs", command)).await;
            }
            network.drain().await;
        }
        let resumed = conversation(&network, &plan).await.unwrap();
        assert_eq!(resumed.conversation_id, running.conversation_id);
        assert_eq!(resumed.history, Some(history));
        // Reconciliation may advance past the program's own failure notice;
        // it must not reset the watermark or admit that notice as human input.
        assert!(resumed.source_cursor >= failed.source_cursor);
        assert_eq!(resumed.admitted_cursor, failed.admitted_cursor);
        assert_eq!(resumed.completed_cursor, failed.completed_cursor);
        assert_eq!(resumed.status, runs::ConversationStatus::Active);
        let recovered = resumed.active_turn.as_ref().unwrap();
        assert_eq!(recovered.turn, previous.turn + 1);
        assert_ne!(recovered.run_id, previous.run_id);
        assert_eq!(recovered.from_cursor, previous.from_cursor);
        assert_eq!(recovered.through_cursor, previous.through_cursor);
        for action in [plan::Control::Pause, plan::Control::Resume] {
            let state = conversation(&network, &plan).await.unwrap();
            for command in
                plan::control_messages(&state, action, &format!("cycle-two-{action:?}")).unwrap()
            {
                network.submit(member(), msg("runs", &command)).await;
            }
            network.drain().await;
        }
        let cycled = conversation(&network, &plan).await.unwrap();
        assert_eq!(cycled.history, resumed.history);
        assert_eq!(cycled.active_turn.unwrap().run_id, recovered.run_id);
    });
}

#[test]
fn missing_explicit_channel_fails_visibly_and_never_creates_a_replacement() {
    block_on(async {
        let mut network = network().await;
        let (plan, program, _) = host_fixture(&mut network, Some("missing")).await;
        provision(&mut network, &plan, &program).await;
        initialize(&mut network, "install").await;
        assert!(conversation(&network, &plan).await.is_none());
        assert!(
            invocations(&network)
                .await
                .iter()
                .any(|entry| matches!(entry.invocation.status, agent::Status::Failed { .. }))
        );
        let reply: chat::ChatReply = query(
            &network,
            "chat",
            chat::ChatQuery::Channel {
                channel_id: "missing".into(),
            },
        )
        .await;
        assert_eq!(reply, chat::ChatReply::Channel(None));
        network
            .submit(
                member(),
                msg(
                    "chat",
                    &chat::ChatMsg::CreateChannel {
                        channel_id: "missing".into(),
                        name: "Restored explicitly".into(),
                        post_policy: chat::PostPolicy::Open,
                    },
                ),
            )
            .await;
        initialize(&mut network, "retry-after-repair").await;
        assert_eq!(
            conversation(&network, &plan).await.unwrap().status,
            runs::ConversationStatus::Active
        );
    });
}
