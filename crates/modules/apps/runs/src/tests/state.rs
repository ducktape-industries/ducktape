use super::*;

#[test]
fn public_message_ids_are_bounded_and_disjoint_for_internal_run_keys() {
    let run_ids = [
        run_id_for("general", 1, "builder"),
        page_run_id_for("thread", 1, "builder"),
        job_run_id_for("job", "builder", 1),
        "attributed/1/builder".into(),
        "attributed/1/builder/post/s0".into(),
        "nested\u{1f}key/post/0".into(),
    ];
    let mut ids = BTreeSet::new();
    for run in run_ids {
        for id in [
            reply_message_id(&run),
            post_message_id(&run, "0"),
            post_message_id(&run, "s0"),
        ] {
            assert!(!id.contains(RUN_KEY_SEPARATOR));
            assert!(
                ids.insert(id),
                "distinct runs and action slots must not alias"
            );
        }
        assert_eq!(reply_message_id(&run).len(), "agent/".len() + 64);
    }
}

#[test]
fn model_run_session_and_request_state_round_trip() {
    let (mut module, registry, run_id) = awaiting_run();
    let mut open = CaptureCtx::new()
        .with_origin(user(8))
        .with_registry(&registry)
        .with_lease_holder(&run_id, &[8; 32]);
    exec(
        &mut module,
        &mut open,
        &admin(&RunsMsg::OpenAgentSession {
            attempt: 0,
            run_id: run_id.clone(),
            session_key: vec![7; 32],
        }),
    )
    .unwrap();
    commit(&mut module);
    let mut act = CaptureCtx::new()
        .with_origin(user(7))
        .with_registry(&registry)
        .with_lease_holder(&run_id, &[8; 32]);
    exec(
        &mut module,
        &mut act,
        &admin(&RunsMsg::AgentAction {
            run_id,
            request_id: "persisted".into(),
            action: create_task("persisted", "a task"),
        }),
    )
    .unwrap();
    commit(&mut module);
    let bytes = module.snapshot();
    let expected = module.root();
    let mut restored = super::module();
    restored.install(&bytes, expected).unwrap();
    assert_eq!(restored.snapshot(), bytes);
    assert_eq!(restored.root(), expected);
    assert_eq!(
        block_on(restored.action_deliveries()).unwrap(),
        block_on(module.action_deliveries()).unwrap()
    );
    let before = restored.snapshot();
    assert!(
        restored
            .install(&bytes[..bytes.len() - 1], expected)
            .is_err()
    );
    assert_eq!(restored.snapshot(), before);
}

#[test]
fn registration_queries_the_deployed_model_program_without_changing_state() {
    let module = RunsModule::new(
        "runs",
        "chat",
        "saga",
        "attribution",
        "dispatch",
        "agent",
        None,
        None,
    );
    let before = module.snapshot();
    for id in ["builder", "another-agent"] {
        let query =
            serde_json::to_vec(&serde_json::json!({"model_program":{"agent_id":id}})).unwrap();
        let reply = block_on(module.query(&query)).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply).unwrap(),
            serde_json::json!({"model_program":crate::model_program(id)})
        );
    }
    assert!(block_on(module.query(br#"{"model_program":{"agent_id":"Invalid ID"}}"#)).is_err());
    assert_eq!(module.snapshot(), before);
}
