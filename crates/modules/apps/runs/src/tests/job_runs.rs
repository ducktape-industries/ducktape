use super::*;

// ---- the jobs lane ----------------------------------------------------------

#[test]
fn a_job_submit_claims_and_dispatches_with_the_spec_payload() {
    let registry = job_registry();
    let mut m = module();
    let mut ctx = CaptureCtx::new()
        .at(3)
        .with_jobs_origin()
        .with_registry(&registry);
    exec(
        &mut m,
        &mut ctx,
        &jobs_event("job-1", "agent/duck", "summarize this work item"),
    )
    .unwrap();
    commit(&mut m);

    // the claim and the dispatch are staged together in the submit block.
    assert_eq!(
        ctx.job_msgs(),
        vec![JobsMsg::Claim {
            job_id: "job-1".into(),
            lease_views: JOB_RUN_LEASE_VIEWS,
        }]
    );
    let run_id = job_run_id_for("job-1", "duck", 3);
    let dispatches = ctx.dispatch_msgs();
    assert_eq!(dispatches.len(), 1);
    let DispatchMsg::Dispatch {
        dispatch_id,
        recipe_id,
        payload,
        ..
    } = &dispatches[0]
    else {
        panic!("expected a dispatch");
    };
    assert_eq!(*dispatch_id, dispatch_id_for(&run_id));
    assert_eq!(*recipe_id, recipe_id_for("duck"));
    let envelope: serde_json::Value =
        serde_json::from_slice(payload).expect("the payload is a JSON envelope");
    assert_eq!(
        envelope["ducktape_run"],
        crate::envelope::RUN_ENVELOPE_MARKER
    );
    assert_eq!(envelope["agent_id"], "duck", "the claiming agent");
    assert!(
        envelope.get("prompt_hash").is_none(),
        "the prompt pin retired — the job run's agent is its curated skills too"
    );
    let conversation = envelope["conversation"].as_str().unwrap();
    assert!(
        conversation.contains("summarize this work item"),
        "the FULL job spec rides the payload"
    );
    assert!(
        envelope["contract"]
            .as_str()
            .unwrap()
            .contains("Return ONLY a JSON object")
    );
    assert!(
        conversation.contains("replies are delivered to this job discussion"),
        "job framing rides along"
    );

    let entry = get_pending(&m, &run_id).expect("job entry staged");
    assert_eq!(entry.job_id, Some("job-1".into()));
    assert_eq!(entry.job_claim_height, 3);
    assert_eq!(entry.agent_id, "duck");
    assert_eq!(entry.requester, RunOrigin::Program(2));
}

#[test]
fn an_oversized_job_spec_is_left_unclaimed_by_the_payload_cap() {
    // the envelope wraps the spec, so a spec at the dispatch cap must
    // overflow it — the job stays on the board, breadcrumb only.
    let registry = job_registry();
    let mut m = module();
    let spec = "x".repeat(MAX_PAYLOAD_BYTES);
    let mut ctx = CaptureCtx::new()
        .at(3)
        .with_jobs_origin()
        .with_registry(&registry);
    exec(&mut m, &mut ctx, &jobs_event("job-1", "agent/duck", &spec)).unwrap();
    assert!(ctx.msgs.is_empty(), "no claim and no dispatch may land");
    let breadcrumbs: Vec<String> = ctx
        .events
        .iter()
        .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
        .collect();
    assert!(
        breadcrumbs
            .iter()
            .any(|b| b.contains("payload exceeds the dispatch cap")),
        "the skip leaves a breadcrumb: {breadcrumbs:?}"
    );
    commit(&mut m);
    assert!(pending_runs(&m).is_empty());
}

#[test]
fn unknown_paused_or_foreign_kind_jobs_are_left_unclaimed() {
    let mut registry = job_registry();
    let mut m = module();

    // an unregistered agent kind: no claim, no dispatch, no entry.
    let mut ctx = CaptureCtx::new()
        .at(2)
        .with_jobs_origin()
        .with_registry(&registry);
    exec(&mut m, &mut ctx, &jobs_event("j", "agent/ghost", "s")).unwrap();
    assert!(ctx.msgs.is_empty());

    // a non-agent kind is somebody else's job.
    let mut ctx = CaptureCtx::new()
        .at(2)
        .with_jobs_origin()
        .with_registry(&registry);
    exec(&mut m, &mut ctx, &jobs_event("j", "render/video", "s")).unwrap();
    assert!(ctx.msgs.is_empty());

    // a paused agent never claims.
    pause(&mut registry, "duck");
    let mut ctx = CaptureCtx::new()
        .at(2)
        .with_jobs_origin()
        .with_registry(&registry);
    exec(&mut m, &mut ctx, &jobs_event("j", "agent/duck", "s")).unwrap();
    assert!(ctx.msgs.is_empty());
    commit(&mut m);
    assert!(pending_runs(&m).is_empty());
}

#[test]
fn a_job_result_finalizes_the_board_and_emits_actions() {
    let registry = job_registry();
    let mut m = module();
    let mut ctx = CaptureCtx::new()
        .at(3)
        .with_jobs_origin()
        .with_registry(&registry);
    exec(&mut m, &mut ctx, &jobs_event("job-1", "agent/duck", "spec")).unwrap();
    commit(&mut m);
    let run_id = job_run_id_for("job-1", "duck", 3);

    let bytes = response(&[], vec![create_task("job-task", "complete job")]);
    let inner = response_json(&[], vec![create_task("job-task", "complete job")]);
    let mut ctx = CaptureCtx::new()
        .at(10)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_claimed_job("job-1", 3);
    exec(&mut m, &mut ctx, &result_event(&run_id, Ok(bytes.clone()))).unwrap();
    commit(&mut m);

    assert_eq!(get_pending(&m, &run_id), None, "the job entry pruned");
    assert_eq!(
        ctx.task_msgs(),
        vec![TaskMsg::CreateTask {
            task_id: "job-task".into(),
            title: "complete job".into(),
            owner: None,
        }]
    );
    let finalize = ctx.job_msgs();
    assert_eq!(finalize.len(), 1);
    let JobsMsg::Finalize {
        job_id,
        ok,
        payload,
    } = &finalize[0]
    else {
        panic!("expected a finalize");
    };
    assert_eq!(job_id, "job-1");
    assert!(*ok);
    // a message-only job result finalizes as a faceted DeliveryReceipt whose
    // `response` is the normalized AgentResponse (no data / output_ref facets).
    let v: serde_json::Value = serde_json::from_str(payload).unwrap();
    assert_eq!(v["ducktape_delivery"], 1);
    assert_eq!(v["status"], "ok");
    assert!(
        v.get("data").is_none(),
        "no data facet on a message-only result"
    );
    assert!(v.get("output_ref").is_none(), "no artifact facet");
    let expected: serde_json::Value = serde_json::from_slice(&inner).unwrap();
    assert_eq!(
        v["response"], expected,
        "response is the normalized AgentResponse"
    );
    assert!(ctx.chat_msgs().is_empty(), "job runs never post to chat");
}

#[test]
fn a_failed_job_result_finalizes_with_error_detail() {
    let registry = job_registry();
    let mut m = module();
    let mut ctx = CaptureCtx::new()
        .at(3)
        .with_jobs_origin()
        .with_registry(&registry);
    exec(&mut m, &mut ctx, &jobs_event("job-1", "agent/duck", "spec")).unwrap();
    commit(&mut m);
    let run_id = job_run_id_for("job-1", "duck", 3);

    let mut ctx = CaptureCtx::new()
        .at(10)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_claimed_job("job-1", 3);
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Err("model unavailable".into())),
    )
    .unwrap();
    commit(&mut m);

    assert_eq!(get_pending(&m, &run_id), None);
    assert!(
        ctx.chat_msgs().is_empty(),
        "a job run has no channel — failures never post to chat"
    );
    assert_eq!(
        ctx.job_msgs(),
        vec![
            JobsMsg::Finalize {
                job_id: "job-1".into(),
                ok: false,
                payload: "model unavailable".into(),
            },
            JobsMsg::Comment {
                created_at_revision: 1,
                job_id: "job-1".into(),
                comment_id: post_message_id(&run_id, "reply"),
                text: "⚠ DUCK failed\n\n```\nmodel unavailable\n```".into(),
            }
        ]
    );
}

#[test]
fn a_job_response_posts_to_its_discussion_and_finalizes_successfully() {
    // A text-only answer belongs to the job discussion and its result receipt.
    let registry = job_registry();
    let mut m = module();
    let mut ctx = CaptureCtx::new()
        .at(3)
        .with_jobs_origin()
        .with_registry(&registry);
    exec(&mut m, &mut ctx, &jobs_event("job-1", "agent/duck", "spec")).unwrap();
    commit(&mut m);
    let run_id = job_run_id_for("job-1", "duck", 3);

    let mut ctx = CaptureCtx::new()
        .at(10)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_claimed_job("job-1", 3);
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(response(&["chatty"], vec![]))),
    )
    .unwrap();
    commit(&mut m);

    assert!(ctx.chat_msgs().is_empty(), "no chat post for a job run");
    let finalize = ctx.job_msgs();
    assert_eq!(finalize.len(), 2);
    assert!(
        matches!(&finalize[1], JobsMsg::Comment { job_id, text, .. } if job_id == "job-1" && text == "chatty")
    );
    let JobsMsg::Finalize { ok, payload, .. } = &finalize[0] else {
        panic!("expected a finalize");
    };
    assert!(*ok, "a text-only answer completes the job");
    assert!(payload.contains("chatty"));
    assert_eq!(get_pending(&m, &run_id), None);
}

#[test]
fn a_stale_job_run_does_not_finalize_a_reclaimed_episode() {
    // the board reclaimed and re-claimed the job at a LATER height: the
    // stale run's delivery must not finalize the new episode.
    let registry = job_registry();
    let mut m = module();
    let mut ctx = CaptureCtx::new()
        .at(3)
        .with_jobs_origin()
        .with_registry(&registry);
    exec(&mut m, &mut ctx, &jobs_event("job-1", "agent/duck", "spec")).unwrap();
    commit(&mut m);
    let run_id = job_run_id_for("job-1", "duck", 3);

    let mut ctx = CaptureCtx::new()
        .at(2000)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_claimed_job("job-1", 1005);
    exec(
        &mut m,
        &mut ctx,
        &result_event(
            &run_id,
            Ok(response(&[], vec![create_task("stale", "late")])),
        ),
    )
    .unwrap();
    commit(&mut m);

    assert!(
        ctx.job_msgs().is_empty(),
        "a stale claim episode is never finalized"
    );
    assert_eq!(
        get_pending(&m, &run_id),
        None,
        "the stale entry still prunes"
    );
}
