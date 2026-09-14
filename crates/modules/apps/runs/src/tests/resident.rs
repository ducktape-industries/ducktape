use super::*;

fn setup(messages: Vec<MessageView>) -> (RunsModule, Registry, CaptureCtx) {
    let registry = registry(&["bot"]);
    let mut module = configured(&registry).with_files_module("files");
    let mut ctx = CaptureCtx::new()
        .at(1)
        .with_origin(Origin::Program(2))
        .with_registry(&registry)
        .with_transcript("general", Vec::new());
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ConfigureConversation {
            conversation_id: "resident".into(),
            agent_id: "bot".into(),
            source: ConversationSource::Channel {
                channel_id: "general".into(),
            },
            history_prefix: "/shared/native/resident".into(),
            session_path: "session.jsonl".into(),
            packages: Vec::new(),
        }),
    )
    .unwrap();
    ctx.transcripts.insert("general".into(), messages);
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ActivateConversation {
            conversation_id: "resident".into(),
            operation_id: "activate".into(),
            active: true,
        }),
    )
    .unwrap();
    ctx.msgs.clear();
    (module, registry, ctx)
}
fn state(module: &RunsModule) -> ConversationView {
    block_on(module.conversation("resident")).unwrap().unwrap()
}

#[test]
fn public_configuration_cannot_preseed_the_job_history_namespace() {
    let registry = registry(&["bot"]);
    let mut module = configured(&registry);
    let mut ctx = CaptureCtx::new()
        .with_origin(Origin::Program(2))
        .with_registry(&registry);
    for (conversation_id, source) in [
        ("job/victim:1", ConversationSource::Detached),
        (
            "fake-job",
            ConversationSource::Job {
                job_id: "victim".into(),
            },
        ),
    ] {
        assert!(
            exec(
                &mut module,
                &mut ctx,
                &admin(&RunsMsg::ConfigureConversation {
                    conversation_id: conversation_id.into(),
                    agent_id: "bot".into(),
                    source,
                    history_prefix: "/shared/native/unrelated".into(),
                    session_path: "session.jsonl".into(),
                    packages: Vec::new()
                })
            )
            .is_err()
        );
        assert!(
            block_on(module.conversation(conversation_id))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn configured_package_survives_unpin_and_history_window_gc() {
    use base64::Engine as _;
    fn file_op(files: &mut files::Files, origin: Origin, height: u64, operation: files::FilesMsg) {
        let mut ctx = sdk_testkit::TestCtx::with_env(sdk::Env {
            origin,
            height,
            consensus_time: height,
            me: "files".into(),
            cause: sdk::Cause::Direct,
        })
        .on_query("identity", |_| {
            Ok(identity::encode_reply(&identity::IdentityReply::Account(
                None,
            )))
        });
        block_on(files.execute(
            &mut ctx,
            &Msg {
                target: "files".into(),
                payload: files::encode_msg(&operation),
            },
        ))
        .unwrap();
        block_on(files.commit_block()).unwrap();
    }
    let directory = tempfile::tempdir().unwrap();
    let mut files = files::Files::open("files", directory.path().to_path_buf()).unwrap();
    files.set_history_window_for_tests(1);
    let bytes = b"export default function extension() {}";
    file_op(
        &mut files,
        Origin::External(vec![1; 32]),
        1,
        files::FilesMsg::Commit {
            base_snapshot: None,
            message: "package".into(),
            changes: vec![
                files::Change::Mkdir {
                    path: "/shared/extension".into(),
                },
                files::Change::Put {
                    path: "/shared/extension/index.js".into(),
                    exec: false,
                    meta: BTreeMap::new(),
                    content: files::Content::Inline {
                        b64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    },
                },
            ],
        },
    );
    let snapshot = files.committed_head_for_test().unwrap();
    file_op(
        &mut files,
        Origin::External(vec![1; 32]),
        2,
        files::FilesMsg::Pin {
            snapshot: snapshot.clone(),
            name: "staging-package".into(),
        },
    );
    let registry = registry(&["bot"]);
    let mut module = configured(&registry).with_files_module("files");
    let mut ctx = CaptureCtx::new()
        .with_origin(Origin::Program(2))
        .with_registry(&registry);
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ConfigureConversation {
            conversation_id: "resident".into(),
            agent_id: "bot".into(),
            source: ConversationSource::Detached,
            history_prefix: "/shared/native/resident".into(),
            session_path: "session.jsonl".into(),
            packages: vec![run_envelope::ConversationPackage {
                name: "extension".into(),
                source_snapshot: snapshot.clone(),
                source_prefix: "/shared/extension".into(),
            }],
        }),
    )
    .unwrap();
    let references: Vec<_> = ctx
        .msgs
        .iter()
        .filter_map(|message| files::decode_msg(&message.payload).ok())
        .collect();
    assert_eq!(references.len(), 1);
    for operation in references {
        file_op(&mut files, Origin::Module("runs".into()), 3, operation);
    }
    file_op(
        &mut files,
        Origin::External(vec![9; 32]),
        4,
        files::FilesMsg::Unpin {
            name: "staging-package".into(),
        },
    );
    file_op(
        &mut files,
        Origin::External(vec![9; 32]),
        5,
        files::FilesMsg::Commit {
            base_snapshot: Some(snapshot.clone()),
            message: "remove live package tree".into(),
            changes: vec![files::Change::Rm {
                path: "/shared/extension".into(),
            }],
        },
    );
    let history = block_on(
        files.query(&files::encode_query(&files::FilesQuery::History {
            limit: 10,
        })),
    )
    .unwrap();
    let files::FilesReply::History(history) = files::decode_reply(&history).unwrap() else {
        panic!("history");
    };
    assert!(
        history.iter().all(|entry| entry.id != snapshot),
        "package snapshot really left the rolling window"
    );
    files.force_gc();
    let result = block_on(files.query(&files::encode_query(&files::FilesQuery::Read {
        path: "/shared/extension/index.js".into(),
        snapshot: Some(snapshot),
        offset: 0,
        len: 1024,
    })))
    .unwrap();
    let files::FilesReply::Read { b64, .. } = files::decode_reply(&result).unwrap() else {
        panic!("retained package read");
    };
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap(),
        bytes
    );
}
fn start(module: &mut RunsModule, ctx: &mut CaptureCtx) -> String {
    exec(
        module,
        ctx,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: "resident".into(),
        }),
    )
    .unwrap();
    exec(
        module,
        ctx,
        &admin(&RunsMsg::RequestConversationTurn {
            conversation_id: "resident".into(),
            turn: 1,
        }),
    )
    .unwrap();
    state(module).active_turn.unwrap().run_id
}
#[test]
fn resident_program_keeps_action_routes_and_admits_structured_source_changes() {
    let one_shot = model_program("bot");
    let resident = conversation_program("bot");
    assert_eq!(one_shot.steps.len(), resident.steps.len());
    let differences: Vec<_> = one_shot
        .steps
        .iter()
        .zip(&resident.steps)
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(
        differences.len(),
        1,
        "only source intake differs; generic action authority is unchanged"
    );
    let agent::Step::Branch {
        test: agent::Predicate::Defined(agent::Value::Ref(path)),
        ..
    } = differences[0].1
    else {
        panic!("resident inlet consumes a committed change");
    };
    assert_eq!(path, &["change".to_string(), "seq".to_string()]);
}
#[test]
fn no_mention_human_posts_are_snapshotted_and_self_replies_do_not_recurse() {
    let messages = vec![
        message(1, "first human"),
        message(2, "second human"),
        message_in("general", 3, Party::Account(2), "self reply", None),
    ];
    let (mut module, _, mut ctx) = setup(messages);
    ctx.env.origin = Origin::Module("chat".into());
    let hook = Msg {
        target: "runs".into(),
        payload: chat::encode_event(&chat::ChatEvent::MessagePosted {
            channel_id: "general".into(),
            seq: 3,
            thread_root: None,
            author: Party::Account(2),
            mentions: Vec::new(),
        }),
    };
    exec(&mut module, &mut ctx, &hook).unwrap();
    let captured = state(&module);
    assert_eq!((captured.source_cursor, captured.admitted_cursor), (3, 2));
    assert!(captured.active_turn.is_none());
    assert!(
        ctx.msgs.is_empty(),
        "source hook must not execute reactions in the source cascade"
    );
    exec(&mut module, &mut ctx, &hook).unwrap();
    assert_eq!(state(&module).admitted_cursor, 2);
    commit(&mut module);
    let events = block_on(module.conversation_events("resident", 1, 64)).unwrap();
    assert_eq!(events.len(), 2);
    let ConversationInput::Chat { message } = &events[0].input else {
        panic!("source body snapshot");
    };
    assert_eq!(message.head.blocks, vec![Block::paragraph("first human")]);
    let root = module.root();
    let mut restored = super::module().with_files_module("files");
    restored.install(&module.snapshot(), root).unwrap();
    assert_eq!(state(&restored), captured);
    assert_eq!(block_on(restored.pending_items()).unwrap().len(), 1);
}
#[test]
fn binding_existing_channel_starts_at_its_watermark_and_missing_source_pauses() {
    let registry = registry(&["bot"]);
    let mut module = configured(&registry);
    let mut ctx = CaptureCtx::new()
        .at(1)
        .with_origin(Origin::Program(2))
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ConfigureConversation {
            conversation_id: "resident".into(),
            agent_id: "bot".into(),
            source: ConversationSource::Channel {
                channel_id: "general".into(),
            },
            history_prefix: "/shared/native/resident".into(),
            session_path: "session.jsonl".into(),
            packages: Vec::new(),
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ActivateConversation {
            conversation_id: "resident".into(),
            operation_id: "activate".into(),
            active: true,
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: "resident".into(),
        }),
    )
    .unwrap();
    assert_eq!(
        (state(&module).source_cursor, state(&module).admitted_cursor),
        (2, 0)
    );
    ctx.transcripts
        .get_mut("general")
        .unwrap()
        .push(message(3, "new member input"));
    ctx.env.origin = Origin::Module("chat".into());
    block_on(module.conversation_chat_hook(
        &ctx,
        &chat::encode_event(&chat::ChatEvent::MessagePosted {
            channel_id: "general".into(),
            seq: 3,
            thread_root: None,
            author: Party::Key(vec![1]),
            mentions: Vec::new(),
        }),
    ))
    .unwrap();
    assert_eq!(
        (state(&module).source_cursor, state(&module).admitted_cursor),
        (3, 1)
    );
    ctx.transcripts.remove("general");
    ctx.env.origin = Origin::Program(2);
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: "resident".into(),
        }),
    )
    .unwrap();
    assert_eq!(
        state(&module).status,
        ConversationStatus::Paused {
            reason: "channel_missing".into()
        }
    );
    assert!(
        !ctx.chat_msgs()
            .iter()
            .any(|message| matches!(message, ChatMsg::CreateChannel { .. })),
        "missing sources are never recreated"
    );
}
#[test]
fn pending_tool_receipt_and_stale_attempt_cannot_advance_resident_turn() {
    let (mut module, registry, mut ctx) = setup(transcript(2));
    let run_id = start(&mut module, &mut ctx);
    let holder = [0xab; 32];
    let session = [0xcd; 32];
    let mut lease = CaptureCtx::new()
        .at(2)
        .with_origin(Origin::External(holder.to_vec()))
        .with_registry(&registry)
        .with_lease_holder(&run_id, &holder);
    exec(
        &mut module,
        &mut lease,
        &admin(&RunsMsg::OpenAgentSession {
            run_id: run_id.clone(),
            attempt: 0,
            session_key: session.to_vec(),
        }),
    )
    .unwrap();
    let checkpoint = RunsMsg::CheckpointConversation {
        conversation_id: "resident".into(),
        run_id: run_id.clone(),
        attempt: 0,
        operation_id: "checkpoint".into(),
        delivery: true,
        history: ConversationHistory {
            revision: 1,
            snapshot: "aa".repeat(32),
        },
    };
    let mut writer = CaptureCtx::new()
        .at(2)
        .with_origin(Origin::External(session.to_vec()))
        .with_registry(&registry)
        .with_lease_holder(&run_id, &holder)
        .with_file(
            "/shared/native/resident/session.jsonl",
            b"{\"native\":true}\n",
        );
    exec(&mut module, &mut writer, &admin(&checkpoint)).unwrap();
    exec(&mut module, &mut writer, &admin(&checkpoint)).unwrap();
    let mut stale = checkpoint.clone();
    let RunsMsg::CheckpointConversation { attempt, .. } = &mut stale else {
        unreachable!();
    };
    *attempt = 1;
    assert!(exec(&mut module, &mut writer, &admin(&stale)).is_err());
    block_on(module.conversation_track_action(None, &run_id, "late".into())).unwrap();
    block_on(module.conversation_model_ended(
        &writer,
        &run_id,
        Some(0),
        RunOutcome::ResultAccepted,
    ))
    .unwrap();
    let turn = state(&module).active_turn.unwrap();
    assert_eq!(turn.phase, ConversationTurnPhase::Draining);
    assert_eq!(state(&module).completed_cursor, 0);
    let before = state(&module);
    block_on(module.conversation_model_ended(
        &writer,
        &run_id,
        Some(0),
        RunOutcome::ResultAccepted,
    ))
    .unwrap();
    assert_eq!(state(&module), before);
}
#[test]
fn native_checkpoint_retention_cas_uses_staged_head_without_tool_budget_or_pin_growth() {
    let (mut module, registry, mut ctx) = setup(transcript(1));
    let run_id = start(&mut module, &mut ctx);
    let holder = [0xab; 32];
    let mut boundary = CaptureCtx::new()
        .at(2)
        .with_origin(Origin::External(holder.to_vec()))
        .with_registry(&registry)
        .with_lease_holder(&run_id, &holder)
        .with_file("/shared/native/resident/session.jsonl", b"native history");
    exec(
        &mut module,
        &mut boundary,
        &admin(&RunsMsg::OpenAgentSession {
            run_id: run_id.clone(),
            attempt: 0,
            session_key: vec![0xcd; 32],
        }),
    )
    .unwrap();
    for revision in 1..=70 {
        exec(
            &mut module,
            &mut boundary,
            &admin(&RunsMsg::CheckpointConversation {
                conversation_id: "resident".into(),
                run_id: run_id.clone(),
                attempt: 0,
                operation_id: format!("checkpoint-{revision}"),
                history: ConversationHistory {
                    revision,
                    snapshot: format!("{revision:064x}"),
                },
                delivery: true,
            }),
        )
        .unwrap();
    }
    let mut previous = None;
    let mut key = None;
    let mut count = 0;
    for message in &boundary.msgs {
        let Ok(files::FilesMsg::CompareExchangeRetention {
            key: current_key,
            expected,
            replacement,
        }) = files::decode_msg(&message.payload)
        else {
            continue;
        };
        assert_eq!(
            expected, previous,
            "CAS must use the earlier checkpoint staged in this same block"
        );
        if let Some(key) = &key {
            assert_eq!(
                &current_key, key,
                "one replacement slot, not per-checkpoint pins"
            );
        }
        key = Some(current_key);
        previous = replacement;
        count += 1;
    }
    assert_eq!(count, 70);
    assert_eq!(module.session(&run_id).unwrap().actions, 0);
    assert_eq!(
        state(&module)
            .active_turn
            .unwrap()
            .checkpoint
            .unwrap()
            .history
            .revision,
        70
    );
    commit(&mut module);
    let before = module.snapshot();
    exec(
        &mut module,
        &mut boundary,
        &admin(&RunsMsg::CheckpointConversation {
            conversation_id: "resident".into(),
            run_id,
            attempt: 0,
            operation_id: "aborted".into(),
            history: ConversationHistory {
                revision: 71,
                snapshot: "ff".repeat(32),
            },
            delivery: true,
        }),
    )
    .unwrap();
    abort(&mut module);
    assert_eq!(
        module.snapshot(),
        before,
        "failed atomic dispatch restores the prior Runs checkpoint"
    );
}
#[test]
fn controller_retry_preserves_logical_input_identity_and_delivered_history() {
    let (mut module, registry, mut ctx) = setup(transcript(1));
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: "resident".into(),
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::RequestConversationTurn {
            conversation_id: "resident".into(),
            turn: 1,
        }),
    )
    .unwrap();
    let original = state(&module).active_turn.unwrap();
    let holder = [0xab; 32];
    let mut native = CaptureCtx::new()
        .with_origin(Origin::External(holder.to_vec()))
        .with_registry(&registry)
        .with_transcript("general", transcript(1))
        .with_lease_holder(&original.run_id, &holder)
        .with_file(
            "/shared/native/resident/session.jsonl",
            b"delivered native tree",
        );
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::OpenAgentSession {
            run_id: original.run_id.clone(),
            attempt: 0,
            session_key: vec![0xcd; 32],
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::CheckpointConversation {
            conversation_id: "resident".into(),
            run_id: original.run_id.clone(),
            attempt: 0,
            operation_id: "delivered".into(),
            history: ConversationHistory {
                revision: 1,
                snapshot: "aa".repeat(32),
            },
            delivery: true,
        }),
    )
    .unwrap();
    native = native.with_lease_attempt(&original.run_id, &holder, 1);
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::OpenAgentSession {
            run_id: original.run_id.clone(),
            attempt: 1,
            session_key: vec![0xce; 32],
        }),
    )
    .unwrap();
    native.env.origin = Origin::Module("dispatch".into());
    exec(
        &mut module,
        &mut native,
        &result_event(&original.run_id, Err("replacement attempt failed".into())),
    )
    .unwrap();
    native.env.origin = Origin::Program(2);
    // The program explicitly refuses the failed attempt's technical reply;
    // that terminal receipt must drain before the controller can recover.
    for request_id in state(&module).active_turn.unwrap().actions {
        native.env.cause = sdk::Cause::Chain {
            root: sdk::Cause::Direct.root_for_item(&sdk::ItemRef {
                source: "agent".into(),
                item: 1,
            }),
            hop: sdk::Hop::Call(sdk::CallId {
                requester: "agent".into(),
                invocation: "refuse-error-reply".into(),
                step: 0,
            }),
        };
        exec(
            &mut module,
            &mut native,
            &admin(&RunsMsg::RejectActionRequest {
                request_id,
                reason: "no_duplicate_notice".into(),
            }),
        )
        .unwrap();
    }
    native.env.cause = sdk::Cause::Direct;
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: "resident".into(),
        }),
    )
    .unwrap();
    assert!(matches!(
        state(&module).status,
        ConversationStatus::Paused { .. }
    ));
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::RetryConversationTurn {
            conversation_id: "resident".into(),
            operation_id: "recover".into(),
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: "resident".into(),
        }),
    )
    .unwrap();
    let retry = state(&module).active_turn.unwrap();
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::RequestConversationTurn {
            conversation_id: "resident".into(),
            turn: retry.turn,
        }),
    )
    .unwrap();
    assert_ne!(original.run_id, retry.run_id);
    assert_eq!(
        conversation_turn_id(original.from_cursor, original.through_cursor),
        conversation_turn_id(retry.from_cursor, retry.through_cursor)
    );
    let payload = native
        .dispatch_msgs()
        .into_iter()
        .find_map(|message| match message {
            DispatchMsg::Dispatch { payload, .. } => Some(payload),
            _ => None,
        })
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        payload["native_conversation"]["turn_id"],
        conversation_turn_id(original.from_cursor, original.through_cursor)
    );
    assert_eq!(
        payload["native_conversation"]["history_snapshot"],
        "aa".repeat(32)
    );
    native.msgs.clear();
    native = native.with_lease_holder(&retry.run_id, &holder);
    native.env.origin = Origin::External(holder.to_vec());
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::OpenAgentSession {
            run_id: retry.run_id.clone(),
            attempt: 0,
            session_key: vec![0xcf; 32],
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::CheckpointConversation {
            conversation_id: "resident".into(),
            run_id: retry.run_id.clone(),
            attempt: 0,
            operation_id: "redelivered".into(),
            history: ConversationHistory {
                revision: 2,
                snapshot: "bb".repeat(32),
            },
            delivery: true,
        }),
    )
    .unwrap();
    native.env.origin = Origin::Module("dispatch".into());
    exec(
        &mut module,
        &mut native,
        &result_event(
            &retry.run_id,
            Ok(runner_wrapper(
                "",
                serde_json::json!({"native_input_handled":true}),
            )),
        ),
    )
    .unwrap();
    native.env.origin = Origin::Program(2);
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: "resident".into(),
        }),
    )
    .unwrap();
    assert_eq!(state(&module).completed_cursor, 1);
    assert!(
        native.msgs.iter().all(|message| message.target != "chat"),
        "event-only restored delivery never invents another chat reply"
    );
    native.transcripts.insert("general".into(), transcript(2));
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: "resident".into(),
        }),
    )
    .unwrap();
    let next = state(&module).active_turn.unwrap();
    assert_eq!(
        conversation_turn_id(next.from_cursor, next.through_cursor),
        "events/1/2"
    );
}

#[test]
fn native_retention_uses_two_replacement_refs_across_multiple_turns() {
    let (mut module, registry, mut ctx) = setup(transcript(3));
    let holder = [0xab; 32];
    let mut references: BTreeMap<String, files::RetentionReference> = BTreeMap::new();
    for turn in 1..=3 {
        exec(
            &mut module,
            &mut ctx,
            &admin(&RunsMsg::ReconcileConversation {
                conversation_id: "resident".into(),
            }),
        )
        .unwrap();
        exec(
            &mut module,
            &mut ctx,
            &admin(&RunsMsg::RequestConversationTurn {
                conversation_id: "resident".into(),
                turn,
            }),
        )
        .unwrap();
        let run_id = state(&module).active_turn.unwrap().run_id;
        let mut native = CaptureCtx::new()
            .at(2)
            .with_origin(Origin::External(holder.to_vec()))
            .with_registry(&registry)
            .with_transcript("general", transcript(3))
            .with_lease_holder(&run_id, &holder)
            .with_file("/shared/native/resident/session.jsonl", b"full native tree");
        exec(
            &mut module,
            &mut native,
            &admin(&RunsMsg::OpenAgentSession {
                run_id: run_id.clone(),
                attempt: 0,
                session_key: vec![0xcd; 32],
            }),
        )
        .unwrap();
        exec(
            &mut module,
            &mut native,
            &admin(&RunsMsg::CheckpointConversation {
                conversation_id: "resident".into(),
                run_id: run_id.clone(),
                attempt: 0,
                operation_id: format!("turn-{turn}"),
                history: ConversationHistory {
                    revision: turn,
                    snapshot: format!("{turn:064x}"),
                },
                delivery: true,
            }),
        )
        .unwrap();
        native.env.origin = Origin::Module("dispatch".into());
        exec(
            &mut module,
            &mut native,
            &result_event(
                &run_id,
                Ok(runner_wrapper(
                    "",
                    serde_json::json!({"native_input_handled":true}),
                )),
            ),
        )
        .unwrap();
        native.env.origin = Origin::Program(2);
        exec(
            &mut module,
            &mut native,
            &admin(&RunsMsg::ReconcileConversation {
                conversation_id: "resident".into(),
            }),
        )
        .unwrap();
        for message in &native.msgs {
            let Ok(files::FilesMsg::CompareExchangeRetention {
                key,
                expected,
                replacement,
            }) = files::decode_msg(&message.payload)
            else {
                continue;
            };
            assert_eq!(
                references.get(&key).cloned(),
                expected,
                "both CAS lanes follow their staged canonical heads"
            );
            references.insert(key, replacement.unwrap());
        }
        assert_eq!(
            references.len(),
            2,
            "old turns are provenance, never additional full-tree GC roots"
        );
    }
    assert_eq!(state(&module).completed_cursor, 3);
    assert!(references.values().all(|reference| reference.revision == 3));
}
#[test]
fn native_event_only_result_requires_delivery_and_does_not_invent_a_reply() {
    let (mut module, registry, mut ctx) = setup(transcript(2));
    let run_id = start(&mut module, &mut ctx);
    let holder = [0xab; 32];
    let mut native = CaptureCtx::new()
        .at(2)
        .with_origin(Origin::External(holder.to_vec()))
        .with_registry(&registry)
        .with_lease_holder(&run_id, &holder)
        .with_file("/shared/native/resident/session.jsonl", b"native history");
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::OpenAgentSession {
            run_id: run_id.clone(),
            attempt: 0,
            session_key: vec![0xcd; 32],
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut native,
        &admin(&RunsMsg::CheckpointConversation {
            conversation_id: "resident".into(),
            run_id: run_id.clone(),
            attempt: 0,
            operation_id: "delivered".into(),
            history: ConversationHistory {
                revision: 1,
                snapshot: "aa".repeat(32),
            },
            delivery: true,
        }),
    )
    .unwrap();
    native.env.origin = Origin::Module("dispatch".into());
    native.msgs.clear();
    exec(
        &mut module,
        &mut native,
        &result_event(
            &run_id,
            Ok(runner_wrapper(
                "",
                serde_json::json!({"native_input_handled":true}),
            )),
        ),
    )
    .unwrap();
    assert!(
        native.chat_msgs().is_empty(),
        "event-only execution is not an assistant message"
    );
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: "resident".into(),
        }),
    )
    .unwrap();
    assert_eq!(state(&module).completed_cursor, 1);
    commit(&mut module);
    assert_eq!(recent_runs(&module)[0].outcome, RunOutcome::ResultAccepted);
    let (mut ordinary, ordinary_registry, ordinary_id) = awaiting_run();
    let mut deliver = CaptureCtx::new()
        .at(9)
        .with_origin(Origin::Module("dispatch".into()))
        .with_registry(&ordinary_registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut ordinary,
        &mut deliver,
        &result_event(
            &ordinary_id,
            Ok(runner_wrapper(
                "",
                serde_json::json!({"native_input_handled":true}),
            )),
        ),
    )
    .unwrap();
    commit(&mut ordinary);
    assert_eq!(
        recent_runs(&ordinary)[0].outcome,
        RunOutcome::Failed,
        "one-shot cannot forge event-only completion"
    );
}
#[test]
fn native_job_is_independent_of_a_caller_run_and_one_shot_jobs_keep_original_transport() {
    let (mut module, mut registry, caller) = awaiting_run();
    registry.extend(job_registry());
    let mut ctx = CaptureCtx::new()
        .at(3)
        .with_jobs_origin()
        .with_registry(&registry)
        .with_claimed_job("worker", 3);
    let job = ctx.jobs.get_mut("worker").unwrap();
    job.status = JobStatus::Pending;
    job.claim = None;
    job.execution = tasks::JobExecution::Conversation;
    exec(
        &mut module,
        &mut ctx,
        &jobs_event("worker", "agent/duck", "native worker"),
    )
    .unwrap();
    let worker = job_run_id_for("worker", "duck", 3);
    let payload = ctx
        .dispatch_msgs()
        .into_iter()
        .find_map(|message| match message {
            DispatchMsg::Dispatch { payload, .. } => Some(payload),
            _ => None,
        })
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        payload["native_conversation"]["conversation_id"],
        "job/worker:1"
    );
    assert_eq!(
        payload["native_conversation"]["events"][0]["input"]["event"]["kind"],
        "job"
    );
    let mut finish = CaptureCtx::new()
        .at(4)
        .with_origin(Origin::Module("dispatch".into()))
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut module,
        &mut finish,
        &result_event(&caller, Err("caller ended".into())),
    )
    .unwrap();
    assert!(
        get_pending(&module, &worker).is_some(),
        "an independent job is not caller-root state"
    );
    assert!(module.delegations.is_empty());
    let mut ordinary = super::module();
    let mut plain = CaptureCtx::new()
        .at(3)
        .with_jobs_origin()
        .with_registry(&job_registry());
    exec(
        &mut ordinary,
        &mut plain,
        &jobs_event("plain", "agent/duck", "one shot"),
    )
    .unwrap();
    let payload = plain
        .dispatch_msgs()
        .into_iter()
        .find_map(|message| match message {
            DispatchMsg::Dispatch { payload, .. } => Some(payload),
            _ => None,
        })
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert!(payload.get("native_conversation").is_none());
}
#[test]
fn managed_discussion_uses_frozen_revision_and_actual_editor_not_comment_author() {
    let (mut module, _, ctx) = setup(Vec::new());
    module.pages = Some("pages".into());
    let snapshot = pages::ManagedDiscussionSnapshot {
        mutation: pages::DiscussionMutation::Edited,
        collection_page_id: "asks".into(),
        page_id: "ask-one".into(),
        comment: pages::Comment {
            id: "comment".into(),
            thread_id: "thread".into(),
            author: pages::Party::Account(1),
            text: "frozen answer".into(),
            mentions: Vec::new(),
            created_at: 1,
            edited_at: Some(2),
            deleted: false,
        },
        thread: pages::DiscussionThreadSnapshot {
            id: "thread".into(),
            target: "ask-one".into(),
            opener: pages::Party::Account(1),
            created_at: 1,
            anchor: None,
            resolved: false,
            resolved_by: None,
        },
    };
    let mut change = attribution::Change {
        seq: 8,
        source: attribution::Source {
            module: "pages".into(),
            kind: "comment".into(),
            object: "comment".into(),
        },
        revision: 2,
        recipient: 2,
        reason: attribution::Reason::Defined(pages::MANAGED_RECORD_COMMENT_REASON.into()),
        kind: attribution::ChangeKind::Added,
        detail: serde_json::to_vec(&snapshot).unwrap(),
        actor: attribution::Actor::Account(9),
        cause: sdk::Cause::Direct,
        height: 2,
    };
    let initial = state(&module);
    block_on(module.admit_conversation_attribution(&ctx, &initial, &change)).unwrap();
    let next = state(&module);
    block_on(module.admit_conversation_attribution(&ctx, &next, &change)).unwrap();
    let events = block_on(module.conversation_events("resident", 1, 64)).unwrap();
    assert_eq!(events.len(), 1);
    let ConversationInput::Event { content, .. } = &events[0].input else {
        panic!("structured attribution");
    };
    assert_eq!(content["source"]["comment"]["text"], "frozen answer");
    assert_eq!(
        content["source"]["comment"]["author"],
        serde_json::json!({"account":1})
    );
    assert_eq!(
        content["attribution"]["actor"],
        serde_json::json!({"account":9})
    );
    change.seq = 9;
    change.actor = attribution::Actor::Account(2);
    block_on(module.admit_conversation_attribution(&ctx, &next, &change)).unwrap();
    assert_eq!(
        state(&module).admitted_cursor,
        1,
        "own program writes must not wake themselves"
    );
}
#[test]
fn native_cancellation_reads_retained_execution_after_real_prune_and_id_reuse() {
    fn source_op(ctx: &mut CaptureCtx, origin: Origin, height: u64, operation: JobsMsg) {
        let source = ctx.jobs_module.as_mut().unwrap();
        let mut source_ctx = sdk_testkit::TestCtx::with_env(Env {
            origin,
            height,
            consensus_time: height,
            me: "jobs".into(),
            cause: sdk::Cause::Direct,
        })
        .on_query("identity", |_| {
            Ok(identity::encode_reply(&identity::IdentityReply::Account(
                None,
            )))
        });
        block_on(source.execute(
            &mut source_ctx,
            &Msg {
                target: "jobs".into(),
                payload: tasks::encode_job_msg(&operation),
            },
        ))
        .unwrap();
        block_on(source.commit_block()).unwrap();
    }
    fn apply_jobs(ctx: &mut CaptureCtx, height: u64) {
        for operation in ctx.job_msgs() {
            source_op(ctx, Origin::Module("runs".into()), height, operation);
        }
        ctx.msgs.clear();
    }
    fn live_job(ctx: &CaptureCtx) -> Option<Job> {
        let bytes = block_on(ctx.query(
            "jobs",
            &tasks::encode_job_query(&JobsQuery::Get {
                job_id: "worker".into(),
            }),
        ))
        .unwrap();
        let JobsReply::Job(job) = tasks::decode_job_reply(&bytes).unwrap() else {
            panic!("job reply");
        };
        job
    }
    let registry = job_registry();
    let mut module = configured(&registry).with_files_module("files");
    let mut ctx = CaptureCtx::new()
        .at(3)
        .with_origin(Origin::Program(2))
        .with_registry(&registry);
    ctx.jobs_module = Some(tasks::Tasks::new(
        "jobs",
        "identity",
        "attribution",
        Box::new(sdk_testkit::MemStore::new()),
    ));
    source_op(
        &mut ctx,
        Origin::Module("submitter".into()),
        1,
        JobsMsg::SubmitConversation {
            job_id: "worker".into(),
            kind: "agent/duck".into(),
            spec: "native work".into(),
        },
    );
    let submitted = live_job(&ctx).unwrap();
    let conversation_id = format!("job/{}", submitted.conversation_id);
    block_on(module.on_jobs_event(
        &mut ctx,
        &jobs_event("worker", "agent/duck", "native work").payload,
    ))
    .unwrap();
    apply_jobs(&mut ctx, 3);
    let run_id = job_run_id_for("worker", "duck", 3);
    let holder = [0xab; 32];
    let history_path = format!(
        "/shared/conversation-history/{}/session.jsonl",
        dispatch_id_for(&conversation_id)
    );
    ctx = ctx
        .with_origin(Origin::External(holder.to_vec()))
        .with_lease_holder(&run_id, &holder)
        .with_file(&history_path, b"cancel history");
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::OpenAgentSession {
            run_id: run_id.clone(),
            attempt: 0,
            session_key: vec![0xcd; 32],
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::CheckpointConversation {
            conversation_id: conversation_id.clone(),
            run_id: run_id.clone(),
            attempt: 0,
            operation_id: "history".into(),
            history: ConversationHistory {
                revision: 1,
                snapshot: "aa".repeat(32),
            },
            delivery: false,
        }),
    )
    .unwrap();
    apply_jobs(&mut ctx, 4);
    source_op(
        &mut ctx,
        Origin::Module("submitter".into()),
        5,
        JobsMsg::Control {
            job_id: "worker".into(),
            operation_id: "cancel".into(),
            input: tasks::JobControlInput::Cancel,
        },
    );
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::AcknowledgeJobControl {
            run_id: run_id.clone(),
            attempt: 0,
            operation_id: "cancel".into(),
        }),
    )
    .unwrap();
    apply_jobs(&mut ctx, 6);
    let settle = admin(&RunsMsg::SettleJobCancellation {
        run_id: run_id.clone(),
        attempt: 0,
        operation_id: "cancel".into(),
        payload: "settled old execution".into(),
    });
    exec(&mut module, &mut ctx, &settle).unwrap();
    assert!(
        !block_on(module.native_cancellation_settled(&ctx, &run_id, Some(0))).unwrap(),
        "an admitted command alone is not terminal proof"
    );
    apply_jobs(&mut ctx, 7);
    let cancelled = live_job(&ctx).unwrap();
    assert_eq!(cancelled.status, JobStatus::Cancelled);
    assert!(
        cancelled.controls[0]
            .acknowledgements
            .iter()
            .any(|ack| ack.attempt == 1)
    );
    assert!(cancelled.native_history.is_some());
    source_op(
        &mut ctx,
        Origin::External(vec![9; 32]),
        8,
        JobsMsg::Prune {
            job_id: "worker".into(),
        },
    );
    assert!(
        live_job(&ctx).is_none(),
        "ordinary terminal prune really removed the live board row"
    );
    let controls = block_on(module.worker_controls(&ctx, &run_id))
        .unwrap()
        .unwrap();
    assert_eq!(controls.job_status, JobStatus::Cancelled);
    assert_eq!(
        controls.result.as_ref().unwrap().payload,
        "settled old execution"
    );
    assert!(block_on(module.native_cancellation_settled(&ctx, &run_id, Some(0))).unwrap());
    assert!(!block_on(module.native_cancellation_settled(&ctx, &run_id, Some(1))).unwrap());
    assert!(!block_on(module.native_cancellation_settled(&ctx, &run_id, None)).unwrap());
    assert!(
        exec(
            &mut module,
            &mut ctx,
            &admin(&RunsMsg::SettleJobCancellation {
                run_id: run_id.clone(),
                attempt: 1,
                operation_id: "cancel".into(),
                payload: "settled old execution".into()
            })
        )
        .is_err()
    );
    exec(&mut module, &mut ctx, &settle).unwrap();
    assert!(
        ctx.job_msgs().is_empty(),
        "completed settlement retry does not re-finalize a pruned job"
    );

    // Inject absent or inconsistent query proofs; neither a local receipt nor
    // an unrelated terminal row may manufacture cancellation acknowledgement.
    let real_source = ctx.jobs_module.take();
    assert!(
        block_on(module.worker_controls(&ctx, &run_id))
            .unwrap()
            .is_none()
    );
    assert!(!block_on(module.native_cancellation_settled(&ctx, &run_id, Some(0))).unwrap());
    let mut wrong = cancelled.clone();
    wrong.attempt += 1;
    ctx.jobs.insert("worker".into(), wrong);
    assert!(!block_on(module.native_cancellation_settled(&ctx, &run_id, Some(0))).unwrap());
    let mut wrong = cancelled.clone();
    wrong.reports[0].payload = "a different settlement".into();
    ctx.jobs.insert("worker".into(), wrong);
    assert!(!block_on(module.native_cancellation_settled(&ctx, &run_id, Some(0))).unwrap());
    let mut wrong = cancelled.clone();
    wrong.created_at_revision += 1;
    ctx.jobs.insert("worker".into(), wrong);
    assert!(
        block_on(module.worker_controls(&ctx, &run_id))
            .unwrap()
            .is_none()
    );
    ctx.jobs.clear();
    ctx.jobs_module = real_source;

    source_op(
        &mut ctx,
        Origin::Module("another_submitter".into()),
        9,
        JobsMsg::Submit {
            job_id: "worker".into(),
            kind: "agent/duck".into(),
            spec: "unrelated replacement".into(),
        },
    );
    let replacement = live_job(&ctx).unwrap();
    assert_ne!(
        replacement.created_at_revision,
        cancelled.created_at_revision
    );
    assert_ne!(replacement.conversation_id, cancelled.conversation_id);
    assert_eq!(replacement.status, JobStatus::Pending);
    assert_eq!(
        block_on(module.worker_controls(&ctx, &run_id))
            .unwrap()
            .unwrap(),
        controls,
        "same JobID is not the same execution"
    );
    assert!(block_on(module.native_cancellation_settled(&ctx, &run_id, Some(0))).unwrap());
    ctx.env.origin = Origin::Module("dispatch".into());
    ctx.msgs.clear();
    exec(
        &mut module,
        &mut ctx,
        &result_event(
            &run_id,
            Ok(runner_wrapper(
                "",
                serde_json::json!({"native_cancelled":true}),
            )),
        ),
    )
    .unwrap();
    assert!(ctx.job_msgs().is_empty());
    assert!(ctx.chat_msgs().is_empty());
    assert_eq!(
        live_job(&ctx).unwrap(),
        replacement,
        "late old result cannot finalize the new incarnation"
    );
    ctx.env.origin = Origin::Program(2);
    exec(
        &mut module,
        &mut ctx,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: conversation_id.clone(),
        }),
    )
    .unwrap();
    commit(&mut module);
    assert_eq!(recent_runs(&module)[0].outcome, RunOutcome::Cancelled);
    assert!(
        block_on(module.conversation(&conversation_id))
            .unwrap()
            .unwrap()
            .active_turn
            .is_none()
    );
    assert!(module.pending_entry(&dispatch_id_for(&run_id)).is_none());
    let mut restored = super::module().with_files_module("files");
    restored.install(&module.snapshot(), module.root()).unwrap();
    assert_eq!(
        block_on(restored.worker_controls(&ctx, &run_id))
            .unwrap()
            .unwrap(),
        controls,
        "terminal readback survives both Runs cleanup/restart and board ID reuse"
    );
}

#[test]
fn safe_boundary_control_and_native_cancellation_require_canonical_job_settlement() {
    let registry = job_registry();
    let mut module = configured(&registry).with_files_module("files");
    let mut intake = CaptureCtx::new()
        .at(3)
        .with_jobs_origin()
        .with_registry(&registry)
        .with_claimed_job("worker", 3);
    let job = intake.jobs.get_mut("worker").unwrap();
    job.execution = tasks::JobExecution::Conversation;
    job.status = JobStatus::Pending;
    exec(
        &mut module,
        &mut intake,
        &jobs_event("worker", "agent/duck", "native work"),
    )
    .unwrap();
    let run_id = job_run_id_for("worker", "duck", 3);
    let conversation_id = "job/worker:1";
    let history_path = format!(
        "/shared/conversation-history/{}/session.jsonl",
        dispatch_id_for(conversation_id)
    );
    let holder = [0xab; 32];
    let mut boundary = CaptureCtx::new()
        .at(4)
        .with_origin(Origin::External(holder.to_vec()))
        .with_registry(&registry)
        .with_lease_holder(&run_id, &holder)
        .with_claimed_job("worker", 3)
        .with_file(&history_path, b"cancel delivery history");
    boundary.jobs.get_mut("worker").unwrap().execution = tasks::JobExecution::Conversation;
    boundary
        .jobs
        .get_mut("worker")
        .unwrap()
        .controls
        .push(tasks::JobControl {
            operation_id: "cancel".into(),
            input: tasks::JobControlInput::Cancel,
            author: tasks::Party::Account(9),
            height: 4,
            acknowledgements: Vec::new(),
        });
    exec(
        &mut module,
        &mut boundary,
        &admin(&RunsMsg::OpenAgentSession {
            run_id: run_id.clone(),
            attempt: 0,
            session_key: vec![0xcd; 32],
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut boundary,
        &admin(&RunsMsg::CheckpointConversation {
            conversation_id: conversation_id.into(),
            run_id: run_id.clone(),
            attempt: 0,
            operation_id: "history".into(),
            history: ConversationHistory {
                revision: 1,
                snapshot: "aa".repeat(32),
            },
            delivery: false,
        }),
    )
    .unwrap();
    exec(
        &mut module,
        &mut boundary,
        &admin(&RunsMsg::AcknowledgeJobControl {
            run_id: run_id.clone(),
            attempt: 0,
            operation_id: "cancel".into(),
        }),
    )
    .unwrap();
    assert!(boundary.job_msgs().iter().any(|msg| matches!(msg, JobsMsg::AcknowledgeControl { operation_id, attempt:1, .. } if operation_id == "cancel")));
    assert!(
        exec(
            &mut module,
            &mut boundary,
            &admin(&RunsMsg::AcknowledgeJobControl {
                run_id: run_id.clone(),
                attempt: 1,
                operation_id: "cancel".into()
            })
        )
        .is_err()
    );
    let settle = admin(&RunsMsg::SettleJobCancellation {
        run_id: run_id.clone(),
        attempt: 0,
        operation_id: "cancel".into(),
        payload: "cancelled at safe boundary".into(),
    });
    exec(&mut module, &mut boundary, &settle).unwrap();
    assert!(
        !block_on(module.native_cancellation_settled(&boundary, &run_id, Some(0))).unwrap(),
        "a submitted settlement is not a canonical outcome"
    );
    let job = boundary.jobs.get_mut("worker").unwrap();
    job.status = JobStatus::Cancelled;
    job.reports.push(tasks::WorkerReport {
        operation_id: "cancel".into(),
        worker: tasks::Party::Module("runs".into()),
        attempt: 1,
        height: 4,
        kind: tasks::WorkerReportKind::Report,
        payload: "cancelled at safe boundary".into(),
    });
    exec(&mut module, &mut boundary, &settle).unwrap();
    assert!(block_on(module.native_cancellation_settled(&boundary, &run_id, Some(0))).unwrap());
    boundary.env.origin = Origin::Module("dispatch".into());
    boundary.msgs.clear();
    exec(
        &mut module,
        &mut boundary,
        &result_event(
            &run_id,
            Ok(runner_wrapper(
                "",
                serde_json::json!({"native_cancelled":true}),
            )),
        ),
    )
    .unwrap();
    assert!(boundary.chat_msgs().is_empty());
    assert!(
        boundary.job_msgs().is_empty(),
        "terminal Jobs are never finalized again"
    );
    boundary.env.origin = Origin::Program(2);
    exec(
        &mut module,
        &mut boundary,
        &admin(&RunsMsg::ReconcileConversation {
            conversation_id: conversation_id.into(),
        }),
    )
    .unwrap();
    let retained = block_on(module.conversation(conversation_id))
        .unwrap()
        .unwrap();
    assert!(retained.active_turn.is_none());
    assert_eq!(retained.history.unwrap().revision, 1);
    commit(&mut module);
    assert_eq!(recent_runs(&module)[0].outcome, RunOutcome::Cancelled);

    // A task keeps its native history, not its previous worker identity.
    let mut next_model = record("different");
    next_model.account = 3;
    let mut next_registry = registry.clone();
    next_registry.insert("different".into(), next_model.clone());
    module.models.insert("different".into(), next_model);
    let mut next_job = boundary.jobs["worker"].clone();
    next_job.job_id = "worker-next".into();
    next_job.conversation_id = "worker:1".into();
    next_job.previous_job_id = Some("worker".into());
    next_job.kind = "agent/different".into();
    next_job.execution = tasks::JobExecution::Conversation;
    next_job.status = JobStatus::Pending;
    next_job.attempt = 0;
    next_job.claim = None;
    next_job.result = None;
    next_job.native_history = None;
    next_job.controls.clear();
    next_job.reports.clear();
    let mut next = CaptureCtx::new()
        .at(5)
        .with_origin(Origin::Program(3))
        .with_registry(&next_registry);
    next.jobs.insert(next_job.job_id.clone(), next_job);
    block_on(module.on_jobs_event(
        &mut next,
        &jobs_event("worker-next", "agent/different", "continue with new worker").payload,
    ))
    .unwrap();
    let payload = next
        .dispatch_msgs()
        .into_iter()
        .find_map(|message| match message {
            DispatchMsg::Dispatch { payload, .. } => Some(payload),
            _ => None,
        })
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(payload["agent_id"], "different");
    assert_eq!(
        payload["native_conversation"]["conversation_id"],
        conversation_id
    );
    assert_eq!(
        payload["native_conversation"]["history_snapshot"],
        "aa".repeat(32)
    );
    let handover = block_on(module.conversation(conversation_id))
        .unwrap()
        .unwrap();
    assert_eq!(
        (handover.agent_id.as_str(), handover.account),
        ("different", 3)
    );
    assert_eq!(
        block_on(module.conversation_events(conversation_id, 1, 2)).unwrap()[0].actor,
        Origin::Program(2)
    );
    assert_eq!(
        block_on(module.conversation_events(conversation_id, 1, 2)).unwrap()[1].actor,
        Origin::Program(3)
    );
}
#[test]
fn failed_deferred_reaction_does_not_roll_back_captured_source() {
    let (mut module, _, mut ctx) = setup(transcript(1));
    ctx.env.origin = Origin::Module("chat".into());
    block_on(module.conversation_chat_hook(
        &ctx,
        &chat::encode_event(&chat::ChatEvent::MessagePosted {
            channel_id: "general".into(),
            seq: 1,
            thread_root: None,
            author: Party::Key(vec![1]),
            mentions: Vec::new(),
        }),
    ))
    .unwrap();
    commit(&mut module);
    let item = block_on(module.pending_items()).unwrap().remove(0);
    let finalizer = sdk_testkit::TestCtx::with_env(sdk::Env {
        height: 2,
        consensus_time: 2,
        origin: Origin::System,
        me: "runs".into(),
        cause: sdk::Cause::Direct,
    });
    let ack = sdk::Ack {
        item: item.item,
        target: "runs".into(),
        outcome: sdk::DeliveryOutcome::Failed {
            reason: "program refused".into(),
        },
    };
    block_on(module.acknowledge_conversation(&finalizer, &ack)).unwrap();
    block_on(module.acknowledge_conversation(&finalizer, &ack)).unwrap();
    commit(&mut module);
    assert_eq!(
        block_on(module.conversation_events("resident", 1, 64))
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        state(&module).status,
        ConversationStatus::Paused { .. }
    ));
}
