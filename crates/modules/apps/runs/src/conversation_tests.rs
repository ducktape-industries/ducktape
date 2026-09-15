use super::*;
use futures::executor::block_on;
use sdk_testkit::TestCtx;

fn state() -> ConversationView {
    ConversationView {
        conversation_id: "room".into(),
        agent_id: "coordinator".into(),
        account: 7,
        source: ConversationSource::Detached,
        history_prefix: "/shared/history/room".into(),
        session_path: "session.jsonl".into(),
        packages: Vec::new(),
        status: ConversationStatus::Active,
        source_cursor: 0,
        admitted_cursor: 0,
        completed_cursor: 0,
        next_turn: 1,
        active_turn: None,
        history: None,
    }
}
fn event(sequence: u64) -> ConversationEvent {
    ConversationEvent {
        sequence,
        operation_id: format!("input-{sequence}"),
        actor: Origin::External(vec![sequence as u8]),
        input: ConversationInput::Control {
            content: format!("human {sequence}"),
        },
        admitted_at: 1,
    }
}
fn transition(state: &mut ConversationView, input: Input) -> Vec<Command> {
    let commands = step(state, input).unwrap();
    for command in &commands {
        if let Command::State(next) = command {
            *state = next.as_ref().clone();
        }
    }
    commands
}
fn checkpoint_for(state: &ConversationView, revision: u64) -> ConversationCheckpoint {
    ConversationCheckpoint {
        run_id: state.active_turn.as_ref().unwrap().run_id.clone(),
        attempt: 1,
        operation_id: format!("checkpoint-{revision}"),
        delivery: true,
        history: ConversationHistory {
            revision,
            snapshot: "a".repeat(64),
        },
    }
}
#[test]
fn two_human_inputs_serialize_and_model_end_does_not_release_ownership() {
    let mut s = state();
    transition(&mut s, Input::Append(event(1)));
    transition(&mut s, Input::Queue);
    let first = s.active_turn.as_ref().unwrap().clone();
    transition(&mut s, Input::Append(event(2)));
    transition(&mut s, Input::Queue);
    assert_eq!(s.active_turn.as_ref().unwrap(), &first);
    transition(&mut s, Input::Requested);
    transition(&mut s, Input::Started);
    let checkpoint = checkpoint_for(&s, 1);
    transition(&mut s, Input::Checkpoint(checkpoint));
    transition(&mut s, Input::Action("late-tool-receipt".into()));
    transition(&mut s, Input::ModelEnded(RunOutcome::ResultAccepted));
    assert!(
        step(&s, Input::Drained(Some(1))).is_err(),
        "tool completion is a separate fact"
    );
    transition(&mut s, Input::ActionsDrained(1));
    transition(&mut s, Input::Drained(Some(1)));
    assert_eq!(s.completed_cursor, 1);
    assert_eq!(s.history.as_ref().unwrap().revision, 1);
    transition(&mut s, Input::Queue);
    let second = s.active_turn.as_ref().unwrap();
    assert_eq!(
        (second.from_cursor, second.through_cursor, second.turn),
        (1, 2, 2)
    );
    assert_ne!(second.run_id, first.run_id);
}
#[test]
fn missing_native_history_pauses_without_consuming_input() {
    let mut s = state();
    transition(&mut s, Input::Append(event(1)));
    transition(&mut s, Input::Queue);
    transition(&mut s, Input::Requested);
    transition(&mut s, Input::Started);
    transition(&mut s, Input::ModelEnded(RunOutcome::Failed));
    transition(&mut s, Input::Drained(Some(1)));
    assert_eq!(s.completed_cursor, 0);
    assert!(matches!(s.status, ConversationStatus::Paused { .. }));
    assert!(s.active_turn.is_some());
}
#[test]
fn checkpoint_revision_and_turn_are_fenced() {
    let mut s = state();
    transition(&mut s, Input::Append(event(1)));
    transition(&mut s, Input::Queue);
    transition(&mut s, Input::Requested);
    transition(&mut s, Input::Started);
    let mut bad = checkpoint_for(&s, 2);
    assert!(step(&s, Input::Checkpoint(bad.clone())).is_err());
    bad.history.revision = 1;
    bad.run_id = "stale-turn".into();
    assert!(step(&s, Input::Checkpoint(bad)).is_err());
}
#[test]
fn failed_new_attempt_keeps_last_native_checkpoint_but_cannot_consume_its_turn() {
    let mut s = state();
    transition(&mut s, Input::Append(event(1)));
    transition(&mut s, Input::Queue);
    transition(&mut s, Input::Requested);
    transition(&mut s, Input::Started);
    let checkpoint = checkpoint_for(&s, 1);
    transition(&mut s, Input::Checkpoint(checkpoint));
    transition(&mut s, Input::ModelEnded(RunOutcome::Failed));
    transition(&mut s, Input::Drained(Some(2)));
    assert_eq!(s.completed_cursor, 0);
    assert!(
        s.active_turn.as_ref().unwrap().checkpoint.is_some(),
        "attempt loss cannot erase committed native history"
    );
    transition(&mut s, Input::Retry);
    assert_eq!(s.history.as_ref().unwrap().revision, 1);
    assert_eq!(s.active_turn.as_ref().unwrap().through_cursor, 1);
}
#[test]
fn duplicate_admission_restart_and_root_authentication() {
    block_on(async {
        let mut module = RunsModule::new(
            "runs",
            "chat",
            "saga",
            "attribution",
            "dispatch",
            "agent",
            None,
            None,
        );
        let initial = state();
        module.write_conversation_state(&initial).unwrap();
        let ctx = TestCtx::with_env(sdk::Env {
            height: 1,
            consensus_time: 1,
            origin: Origin::Program(7),
            me: "runs".into(),
            cause: sdk::Cause::Direct,
        });
        let input = ConversationInput::Control {
            content: "human input".into(),
        };
        module
            .append_conversation_input(&ctx, "room".into(), "stable-op".into(), input.clone())
            .await
            .unwrap();
        module
            .append_conversation_input(&ctx, "room".into(), "stable-op".into(), input)
            .await
            .unwrap();
        assert_eq!(
            module
                .require_conversation("room")
                .await
                .unwrap()
                .admitted_cursor,
            1
        );
        assert!(
            module
                .append_conversation_input(
                    &ctx,
                    "room".into(),
                    "stable-op".into(),
                    ConversationInput::Control {
                        content: "different".into()
                    }
                )
                .await
                .is_err()
        );
        module.commit_block().await.unwrap();
        let root = module.root();
        let snapshot = module.snapshot();
        let mut restored = RunsModule::new(
            "runs",
            "chat",
            "saga",
            "attribution",
            "dispatch",
            "agent",
            None,
            None,
        );
        restored.install(&snapshot, root).unwrap();
        assert_eq!(restored.root(), root);
        assert_eq!(
            restored.conversation("room").await.unwrap(),
            module.conversation("room").await.unwrap()
        );
        assert_eq!(
            restored
                .conversation_events("room", 1, 64)
                .await
                .unwrap()
                .len(),
            1
        );
        let mut invalid = module.receipts.snapshot();
        let mut bad = module.require_conversation("room").await.unwrap();
        bad.admitted_cursor = 2;
        invalid.insert(key("state", "room"), sdk::wire::encode(&bad));
        assert!(
            validate_records(&invalid).is_err(),
            "missing source events fail closed even under a recomputed hash"
        );
    });
}
#[test]
fn duplicate_model_settlement_and_job_continuation_keep_history() {
    let mut s = state();
    s.source = ConversationSource::Job {
        job_id: "first-job".into(),
    };
    transition(
        &mut s,
        Input::JobStarted {
            run_id: "first-run".into(),
            event: event(1),
        },
    );
    let checkpoint = checkpoint_for(&s, 1);
    transition(&mut s, Input::Checkpoint(checkpoint));
    transition(&mut s, Input::ModelEnded(RunOutcome::ResultAccepted));
    let ended = s.clone();
    transition(&mut s, Input::ModelEnded(RunOutcome::ResultAccepted));
    assert_eq!(s, ended);
    transition(&mut s, Input::Drained(Some(1)));
    transition(
        &mut s,
        Input::JobStarted {
            run_id: "continuation-run".into(),
            event: event(2),
        },
    );
    assert_eq!(s.history.as_ref().unwrap().revision, 1);
    assert_eq!(s.active_turn.as_ref().unwrap().run_id, "continuation-run");
    assert_eq!(s.active_turn.as_ref().unwrap().turn, 2);
}
#[test]
fn conversation_administration_tracks_current_identity_controller() {
    block_on(async {
        let mut module = RunsModule::new(
            "runs",
            "chat",
            "saga",
            "attribution",
            "dispatch",
            "agent",
            None,
            None,
        );
        module.write_conversation_state(&state()).unwrap();
        let context = |signer: u8, controller: u64| {
            TestCtx::with_env(sdk::Env {
                height: 1,
                consensus_time: 1,
                origin: Origin::External(vec![signer]),
                me: "runs".into(),
                cause: sdk::Cause::Direct,
            })
            .on_query("identity", move |bytes| {
                let (number, control) =
                    match identity::decode_query(bytes).map_err(Error::Module)? {
                        identity::IdentityQuery::Get { number } => (
                            number,
                            identity::Control::Program {
                                controller,
                                executor: "agent".into(),
                                generation: 1,
                                standing: identity::ProgramStanding::Active,
                            },
                        ),
                        identity::IdentityQuery::OfKey { key } => {
                            (u64::from(key[0]), identity::Control::Keys)
                        }
                        _ => return Err(Error::QueryUnsupported),
                    };
                Ok(identity::encode_reply(&identity::IdentityReply::Account(
                    Some(identity::AccountView {
                        number,
                        name: "member".into(),
                        control,
                        keys: Vec::new(),
                        avatar: None,
                        bio: None,
                        updated_at: 1,
                    }),
                )))
            })
        };
        module
            .activate_conversation(&context(9, 9), "room".into(), "first".into(), true)
            .await
            .unwrap();
        assert!(
            module
                .activate_conversation(&context(9, 8), "room".into(), "first".into(), true)
                .await
                .is_err(),
            "a historical controller cannot replay its old capability"
        );
        module
            .activate_conversation(
                &context(8, 8),
                "room".into(),
                "new-controller".into(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            module.require_conversation("room").await.unwrap().status,
            ConversationStatus::Inactive
        );
    });
}
#[test]
fn scheduled_input_uses_network_clock_replaces_cancels_and_fires_once() {
    block_on(async {
        for (unit, due_at) in [
            (sdk::genesis_config::TimeUnit::Height, 110),
            (sdk::genesis_config::TimeUnit::Millis, 10_100),
        ] {
            let mut module = RunsModule::new(
                "runs",
                "chat",
                "saga",
                "attribution",
                "dispatch",
                "agent",
                None,
                None,
            )
            .with_time_unit(unit);
            module.write_conversation_state(&state()).unwrap();
            let ctx = TestCtx::with_env(sdk::Env {
                height: 1,
                consensus_time: 100,
                origin: Origin::Program(7),
                me: "runs".into(),
                cause: sdk::Cause::Direct,
            });
            let input = ConversationInput::Event {
                kind: "checkin".into(),
                content: serde_json::json!({}),
            };
            module
                .schedule_conversation_input(
                    &ctx,
                    "room".into(),
                    "schedule".into(),
                    "checkin".into(),
                    Some(10),
                    input.clone(),
                )
                .await
                .unwrap();
            module
                .schedule_conversation_input(
                    &ctx,
                    "room".into(),
                    "schedule".into(),
                    "checkin".into(),
                    Some(10),
                    input.clone(),
                )
                .await
                .unwrap();
            assert_eq!(
                module.conversation_schedules("room").await.unwrap()[0].status,
                ConversationScheduleStatus::Pending { due_at }
            );
            let before_due = TestCtx::at_height(due_at - 1);
            module.crank_conversation_inputs(&before_due).await.unwrap();
            assert_eq!(
                module
                    .require_conversation("room")
                    .await
                    .unwrap()
                    .admitted_cursor,
                0
            );
            module.commit_block().await.unwrap();
            let mut restored = RunsModule::new(
                "runs",
                "chat",
                "saga",
                "attribution",
                "dispatch",
                "agent",
                None,
                None,
            )
            .with_time_unit(unit);
            restored.install(&module.snapshot(), module.root()).unwrap();
            let due = TestCtx::at_height(due_at);
            restored.crank_conversation_inputs(&due).await.unwrap();
            restored.crank_conversation_inputs(&due).await.unwrap();
            assert_eq!(
                restored
                    .require_conversation("room")
                    .await
                    .unwrap()
                    .admitted_cursor,
                1
            );
            assert_eq!(
                restored.conversation_events("room", 1, 1).await.unwrap()[0].actor,
                Origin::Program(7)
            );
            restored
                .schedule_conversation_input(
                    &ctx,
                    "room".into(),
                    "replace".into(),
                    "checkin".into(),
                    Some(20),
                    input.clone(),
                )
                .await
                .unwrap();
            restored
                .schedule_conversation_input(
                    &ctx,
                    "room".into(),
                    "cancel".into(),
                    "checkin".into(),
                    None,
                    input,
                )
                .await
                .unwrap();
            restored
                .crank_conversation_inputs(&TestCtx::at_height(u64::MAX))
                .await
                .unwrap();
            assert_eq!(
                restored
                    .require_conversation("room")
                    .await
                    .unwrap()
                    .admitted_cursor,
                1
            );
            assert_eq!(
                restored.conversation_schedules("room").await.unwrap()[0].status,
                ConversationScheduleStatus::Cancelled
            );
        }
    });
}
#[test]
fn explicit_recovery_never_reopens_a_terminal_worker_job() {
    let mut s = state();
    s.source = ConversationSource::Job {
        job_id: "original".into(),
    };
    transition(
        &mut s,
        Input::JobStarted {
            run_id: "run".into(),
            event: event(1),
        },
    );
    transition(&mut s, Input::ModelEnded(RunOutcome::Failed));
    transition(&mut s, Input::Drained(Some(1)));
    transition(&mut s, Input::Retry);
    assert!(
        s.active_turn.is_none(),
        "a new Job must authorize the next independent execution"
    );
    assert_eq!(s.completed_cursor, 1);
}
fn assert_dispatch_shape(source: &str, name: &str) {
    let file = syn::parse_file(source).unwrap();
    let function = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == name => Some(function),
            _ => None,
        })
        .unwrap();
    assert_eq!(function.block.stmts.len(), 1);
    let syn::Stmt::Expr(syn::Expr::Match(dispatch), None) = &function.block.stmts[0] else {
        panic!("step must be only a match");
    };
    for arm in &dispatch.arms {
        assert!(arm.guard.is_none());
        assert!(!matches!(arm.pat, syn::Pat::Wild(_)));
        assert!(
            matches!(*arm.body, syn::Expr::Call(_)),
            "each input delegates exactly once"
        );
    }
}
#[test]
fn dispatch_is_one_match_with_one_handler_per_variant() {
    assert_dispatch_shape(include_str!("conversations.rs"), "step");
    assert_dispatch_shape(include_str!("conversation_schedule.rs"), "schedule_step");
}
