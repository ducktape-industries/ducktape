use super::*;

// ---- the result intake ----------------------------------------------------------

#[test]
fn a_valid_response_emits_the_reply_and_actions_and_prunes_the_entry() {
    let (mut m, registry, run_id) = awaiting_run();
    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut m,
        &mut ctx,
        &result_event(
            &run_id,
            Ok(response(
                &["on it"],
                vec![
                    create_task("t1", "ship it"),
                    // updating a task created earlier in this SAME response
                    // is valid — tasks applies the follow-ups in order.
                    update_task_status("t1", "in_progress"),
                ],
            )),
        ),
    )
    .unwrap();
    commit(&mut m);

    assert_eq!(
        get_pending(&m, &run_id),
        None,
        "the delivered entry pruned — the dispatch module holds the history"
    );
    assert_eq!(
        ctx.chat_msgs(),
        vec![ChatMsg::PostMessage {
            channel_id: "general".into(),
            message_id: reply_message_id(&run_id),
            blocks: vec![Block::paragraph("on it")],
            thread: Some(2),
        }],
        "the reply posts as the AGENT, under the run's message id"
    );
    assert_eq!(
        ctx.task_msgs(),
        vec![
            TaskMsg::CreateTask {
                task_id: "t1".into(),
                title: "ship it".into(),
                owner: None,
            },
            TaskMsg::UpdateStatus {
                task_id: "t1".into(),
                status: TaskStatus::InProgress,
            },
        ]
    );
}

#[test]
fn a_threaded_anchor_threads_the_reply() {
    let registry = registry(&["bot"]);
    let mut m = configured(&registry);
    // seq 3 is a reply to root 1; the pin records thread_root = 1.
    let mut thread_transcript = transcript(2);
    thread_transcript.push(message_in(
        "general",
        3,
        Party::Key(vec![1; 32]),
        "in thread",
        Some(1),
    ));
    let mut ctx = CaptureCtx::new()
        .at(3)
        .with_program_origin()
        .with_registry(&registry)
        .with_transcript("general", thread_transcript.clone());
    exec(&mut m, &mut ctx, &engagement("general", 3, vec![])).unwrap();
    commit(&mut m);
    let run_id = run_id_for("general", 3, "bot");
    assert_eq!(get_pending(&m, &run_id).unwrap().thread_root, Some(1));
    let mut ctx = CaptureCtx::new()
        .at(9)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", thread_transcript);
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(response(&["answered"], vec![]))),
    )
    .unwrap();
    commit(&mut m);
    let posts = ctx.chat_msgs();
    assert_eq!(posts.len(), 1);
    let ChatMsg::PostMessage { thread, .. } = &posts[0] else {
        panic!("expected a post");
    };
    assert_eq!(*thread, Some(1), "the reply joins the anchor's thread");
}

#[test]
fn invalid_responses_fail_the_run_and_surface_a_threaded_failure_reply() {
    // normalization already absorbed shape problems (prose, fences,
    // oversize); what remains failable is POLICY: task validity. every
    // case emits NO follow-up except the ⚠ failure reply, leaves a
    // breadcrumb, and prunes the entry — never the block.
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "task already exists: t0",
            response(&["ok"], vec![create_task("t0", "dup of a committed task")]),
        ),
        (
            "task already exists: fresh",
            response(
                &["ok"],
                vec![create_task("fresh", "one"), create_task("fresh", "two")],
            ),
        ),
        (
            "unknown task: ghost",
            response(&["ok"], vec![update_task_status("ghost", "done")]),
        ),
        (
            "unknown task status",
            response(&["ok"], vec![update_task_status("t0", "shipped")]),
        ),
        (
            "task_id must be non-empty",
            response(&["ok"], vec![create_task(String::new(), "x")]),
        ),
    ];
    for (fragment, bytes) in cases {
        let (mut m, registry, run_id) = awaiting_run();
        let mut ctx = CaptureCtx::new()
            .at(8)
            .with_dispatch_origin()
            .with_registry(&registry)
            .with_transcript("general", transcript(2))
            .with_task("t0");
        exec(&mut m, &mut ctx, &result_event(&run_id, Ok(bytes))).unwrap();
        assert!(
            ctx.task_msgs().is_empty(),
            "an invalid response must emit no task writes ({fragment})"
        );
        let posts = ctx.chat_msgs();
        assert_eq!(posts.len(), 1, "exactly one failure reply ({fragment})");
        let ChatMsg::PostMessage {
            message_id, blocks, ..
        } = &posts[0]
        else {
            panic!("expected a post");
        };
        assert_eq!(
            *message_id,
            reply_message_id(&run_id),
            "the failure reply holds the run's one reply id ({fragment})"
        );

        assert_eq!(
            blocks.len(),
            2,
            "one ⚠ paragraph and the reason's code block ({fragment})"
        );
        let Block::Paragraph(spans) = &blocks[0] else {
            panic!("expected a paragraph");
        };
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, "⚠ BOT failed", "the reply names the agent's display name");
        let Block::Code { text: detail, .. } = &blocks[1] else {
            panic!("expected the reason in a code block");
        };
        assert!(
            detail.contains(fragment),
            "the code block carries the reason: {detail}"
        );
        let breadcrumbs: Vec<String> = ctx
            .events
            .iter()
            .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
            .collect();
        assert!(
            breadcrumbs.iter().any(|b| b.contains(fragment)),
            "breadcrumbs {breadcrumbs:?} must mention {fragment:?}"
        );
        commit(&mut m);
        assert_eq!(
            get_pending(&m, &run_id),
            None,
            "the failed run's entry pruned ({fragment})"
        );
    }
}

#[test]
fn a_task_id_tasks_would_reject_fails_the_run_not_the_op() {
    // tasks admits an id through `sdk::validate_id` (MAX_TASK_ID bytes, no
    // KEY_SEP), and a model-authored id is bounded only by MAX_ACTIONS_BYTES.
    // an id tasks would REJECT must fail the run here: rejected at apply, the
    // follow-up would roll back the whole settle op — reply, finalize, run
    // transition — on every replay, with no breadcrumb to show for it.
    let cases = [
        (
            "t".repeat(tasks::MAX_TASK_ID + 1),
            format!("the cap is {}", tasks::MAX_TASK_ID),
        ),
        (
            format!("t{}1", sdk::KEY_SEP),
            "reserved separator".to_owned(),
        ),
    ];
    for (task_id, fragment) in cases {
        let (mut m, registry, run_id) = awaiting_run();
        let mut ctx = CaptureCtx::new()
            .at(8)
            .with_dispatch_origin()
            .with_registry(&registry)
            .with_transcript("general", transcript(2));
        exec(
            &mut m,
            &mut ctx,
            &result_event(
                &run_id,
                Ok(response(
                    &["ok"],
                    vec![create_task(task_id, "rejected at tasks")],
                )),
            ),
        )
        .expect("the settle op is ACCEPTED — the RUN fails, not the op");
        assert!(
            ctx.task_msgs().is_empty(),
            "no task follow-up is emitted ({fragment})"
        );
        assert_eq!(
            ctx.chat_msgs().len(),
            1,
            "only the ⚠ failure reply posts ({fragment})"
        );
        let breadcrumbs: Vec<String> = ctx
            .events
            .iter()
            .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
            .collect();
        assert!(
            breadcrumbs.iter().any(|b| b.contains(&fragment)),
            "the fail_run breadcrumb names the constraint: {breadcrumbs:?}"
        );
        commit(&mut m);
        assert_eq!(
            get_pending(&m, &run_id),
            None,
            "the failed run's entry pruned ({fragment})"
        );
    }
}

#[test]
fn a_task_id_at_the_cap_still_emits() {
    // the boundary belongs to the ACCEPTING side: tasks admits exactly
    // MAX_TASK_ID bytes, so this validator must not be one byte stricter.
    let task_id = "t".repeat(tasks::MAX_TASK_ID);
    let (mut m, registry, run_id) = awaiting_run();
    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut m,
        &mut ctx,
        &result_event(
            &run_id,
            Ok(response(
                &["on it"],
                vec![create_task(task_id.clone(), "at the cap")],
            )),
        ),
    )
    .unwrap();
    assert_eq!(
        ctx.task_msgs(),
        vec![TaskMsg::CreateTask {
            task_id,
            title: "at the cap".into(),
            owner: None,
        }]
    );
}

#[test]
fn raw_model_text_normalizes_into_a_postable_reply() {
    // the oracle wraps the model's RAW text in the runner result; the
    // intake's deterministic normalization turns prose, empty JSON, and
    // oversized answers into valid replies instead of failed runs.
    let cases: Vec<Vec<u8>> = vec![
        runner_wrapper("just prose, no JSON anywhere", serde_json::json!({})),
        response(&[], vec![]),
        runner_wrapper(
            &"x".repeat(MAX_REPLY_BLOCKS_BYTES + 1),
            serde_json::json!({}),
        ),
    ];
    for bytes in cases {
        let (mut m, registry, run_id) = awaiting_run();
        let mut ctx = CaptureCtx::new()
            .at(8)
            .with_dispatch_origin()
            .with_registry(&registry)
            .with_transcript("general", transcript(2));
        exec(&mut m, &mut ctx, &result_event(&run_id, Ok(bytes))).unwrap();
        commit(&mut m);
        assert_eq!(get_pending(&m, &run_id), None, "the entry pruned");
        let posts = ctx.chat_msgs();
        assert_eq!(posts.len(), 1, "exactly one normalized reply posts");
    }
}

#[test]
fn oversized_actions_fail_the_run_deterministically() {
    // the byte peer of the count cap: one action carrying a huge payload
    // (a pasted-file title) must be a deterministic run failure — never an
    // oversized finalize payload the jobs board would byte-truncate into
    // invalid JSON.
    let (mut m, registry, run_id) = awaiting_run();
    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    let huge = response(&[], vec![create_task("t1", "x".repeat(MAX_ACTIONS_BYTES))]);
    exec(&mut m, &mut ctx, &result_event(&run_id, Ok(huge))).unwrap();
    commit(&mut m);
    assert_eq!(
        get_pending(&m, &run_id),
        None,
        "the failed run's entry pruned"
    );
    let breadcrumbs: Vec<String> = ctx
        .events
        .iter()
        .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
        .collect();
    assert!(
        breadcrumbs
            .iter()
            .any(|b| b.contains(&format!("the cap is {MAX_ACTIONS_BYTES}"))),
        "the failure names the byte cap: {breadcrumbs:?}"
    );
}

#[test]
fn an_over_cap_action_set_is_refused_not_truncated() {
    // the COUNT peer of the test above, and the reason normalization no longer
    // truncates: a response carrying one action past the cap must fail the run
    // BY NAME — in the breadcrumb and in the chat reply the run posts — instead
    // of silently losing its tail and delivering a partial action set the agent
    // never sees dropped.
    let over_cap = MAX_ACTIONS_PER_RUN + 1;
    let (mut m, registry, run_id) = awaiting_run();
    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    let actions = (0..over_cap)
        .map(|n| create_task(format!("t{n}"), format!("task {n}")))
        .collect();
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(response(&["on it"], actions))),
    )
    .unwrap();
    commit(&mut m);

    let cap_sentence = format!("{over_cap} actions exceed the cap of {MAX_ACTIONS_PER_RUN}");
    let breadcrumbs: Vec<String> = ctx
        .events
        .iter()
        .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
        .collect();
    assert!(
        breadcrumbs.iter().any(|b| b.contains(&cap_sentence)),
        "the validator's own count check is reachable: {breadcrumbs:?}"
    );
    let posted = ctx
        .chat_msgs()
        .into_iter()
        .filter_map(|msg| match msg {
            ChatMsg::PostMessage { blocks, .. } => Some(format!("{blocks:?}")),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        posted.iter().any(|text| text.contains(&cap_sentence)),
        "the reply says the action set was refused for exceeding the cap: {posted:?}"
    );
    assert!(
        ctx.task_msgs().is_empty(),
        "an over-cap set delivers NO action — never the first eight"
    );
}

#[test]
fn code_blocks_survive_normalization_into_chat_blocks() {
    let (mut m, registry, run_id) = awaiting_run();
    let raw = r#"{"reply_blocks":[{"id":"b1","kind":"paragraph","text":"hello"},{"kind":"code","lang":"rust","text":"fn main() {}"},{"kind":"Alien","text":"dropped"},{"kind":"paragraph","text":"  "}],"actions":[]}"#;
    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(runner_wrapper(raw, serde_json::json!({})))),
    )
    .unwrap();
    let posts = ctx.chat_msgs();
    let ChatMsg::PostMessage { blocks, .. } = &posts[0] else {
        panic!("expected a post");
    };
    assert_eq!(
        *blocks,
        vec![
            Block::paragraph("hello"),
            Block::Code {
                lang: Some("rust".into()),
                text: "fn main() {}".into(),
            },
        ],
        "known kinds map to chat blocks; unknown kinds and blank texts drop"
    );
}

#[test]
fn a_fenced_json_reply_is_parsed_into_prose_not_dumped_as_a_code_block() {
    // the observed failure: an agentic CLI wraps its AgentResponse in a
    // ```json fence despite the contract, the bare parse fails, and the
    // whole fenced string lands in chat as a raw code block. the tolerant
    // parser must recover the real prose.
    let (mut m, registry, run_id) = awaiting_run();
    let raw = "```json\n{\"reply_blocks\":[{\"kind\":\"paragraph\",\"text\":\"QUACKTEST! Hello there.\"}],\"actions\":[]}\n```";
    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(runner_wrapper(raw, serde_json::json!({})))),
    )
    .unwrap();
    let posts = ctx.chat_msgs();
    let ChatMsg::PostMessage { blocks, .. } = &posts[0] else {
        panic!("expected a post");
    };
    assert_eq!(
        *blocks,
        vec![Block::paragraph("QUACKTEST! Hello there.")],
        "a fenced AgentResponse is decoded to its prose, never posted as raw JSON"
    );
}

#[test]
fn parse_strict_response_tolerates_the_shapes_llms_actually_emit() {
    let bare = r#"{"reply_blocks":[{"kind":"paragraph","text":"hi"}],"actions":[]}"#;
    assert_eq!(
        parse_strict_response(bare).unwrap().reply_blocks[0].text,
        "hi"
    );

    // a fence with an info string (```json), the reproduced case.
    let fenced = format!("```json\n{bare}\n```");
    assert_eq!(
        parse_strict_response(&fenced).unwrap().reply_blocks[0].text,
        "hi"
    );

    // a bare fence (```), no info string.
    let bare_fence = format!("```\n{bare}\n```");
    assert_eq!(
        parse_strict_response(&bare_fence).unwrap().reply_blocks[0].text,
        "hi"
    );

    // JSON with a trailing line of prose the model tacked on.
    let trailing = format!("{fenced}\nHope that helps!");
    assert_eq!(
        parse_strict_response(&trailing).unwrap().reply_blocks[0].text,
        "hi"
    );

    // genuine prose (no JSON object) does NOT parse — it must fall back to
    // the raw-text paragraph, not be swallowed.
    assert!(parse_strict_response("just a plain hello, no json here").is_none());
    assert!(parse_strict_response("   ").is_none());

    // a `}` before the first `{` must not panic the outermost-object span.
    assert!(parse_strict_response("close } then open { please").is_none());
}

#[test]
fn a_fenced_response_retains_reply_blocks_and_actions() {
    // Parsing preserves both facets; delivery resolves the source.
    let raw = "```json\n{\"reply_blocks\":[{\"kind\":\"paragraph\",\"text\":\"noise\"}],\"actions\":[{\"operation\":\"tasks.create\",\"input\":{\"task_id\":\"t1\",\"title\":\"did it\"}}]}\n```";
    let parsed = agent_response_from_text(raw);
    assert!(
        parsed.reply_blocks[0].text == "noise",
        "source routing must not discard the answer"
    );
    assert_eq!(parsed.actions.len(), 1, "the fenced action is recovered");
}

// ---- chat.post_message on the settle path ------------------------------------

#[test]
fn a_post_message_action_lands_agent_authored_under_a_deterministic_id() {
    // the agent SPEAKING (its own channel, its own message) rather than
    // ANSWERING where it was engaged — one more action in the strict lane, and
    // one more proposed chat post for the program account.
    let (mut m, registry, run_id) = awaiting_run();
    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut m,
        &mut ctx,
        &result_event(
            &run_id,
            Ok(response(
                &["done"],
                vec![post_message("general", "progress: halfway", None)],
            )),
        ),
    )
    .unwrap();
    commit(&mut m);

    let msgs = ctx.chat_msgs();
    assert_eq!(msgs.len(), 2, "the run's reply AND the agent's own post");
    assert_eq!(
        msgs[1],
        ChatMsg::PostMessage {
            channel_id: "general".into(),
            // the settle path numbers by the action's index in the response —
            // disjoint from the session lane's `s`-prefixed slots.
            message_id: post_message_id(&run_id, "0"),
            blocks: vec![Block::paragraph("progress: halfway")],
            thread: None,
        }
    );
    assert_ne!(
        msgs[1], msgs[0],
        "the post never squats the run's ONE reply id"
    );
}

#[test]
fn a_post_message_action_decodes_and_threads() {
    // a `chat.post_message` action in the response prose threads under the
    // named root — the prose-parsed action lane (the production path).
    let (mut m, registry, run_id) = awaiting_run();
    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    let prose = String::from_utf8(response_json(
        &["done"],
        vec![post_message("general", "threaded update", Some(1))],
    ))
    .unwrap();
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(runner_wrapper(&prose, serde_json::json!({})))),
    )
    .unwrap();

    let msgs = ctx.chat_msgs();
    assert_eq!(msgs.len(), 2);
    assert!(
        matches!(
            &msgs[1],
            ChatMsg::PostMessage { thread: Some(1), channel_id, .. } if channel_id == "general"
        ),
        "the effect's thread rides through: {:?}",
        msgs[1]
    );
}

/// the dispatch sibling read runs' session + settle lanes make (a committed
/// `DispatchView` lookup) round-trips through the shared `sdk_testkit::TestCtx`
/// `on_query` seam — the programmable sibling response the per-crate
/// `CaptureCtx` doubles had to hand-roll, driven here with runs' real dispatch
/// wire types.
#[test]
fn dispatch_view_reads_through_testkit_on_query() {
    use sdk_testkit::TestCtx;

    let dispatch_id = "d-runs-1";
    let ctx = TestCtx::at_height(5).on_query("dispatch", |req| {
        let dispatch::DispatchQuery::Dispatch {
            receiver,
            dispatch_id,
        } = dispatch::decode_query(req).map_err(Error::Module)?
        else {
            return Err(Error::QueryUnsupported);
        };
        assert_eq!(receiver, "runs", "runs is the dispatching module");
        Ok(dispatch::encode_reply(&dispatch::DispatchReply::Dispatch(
            Some(DispatchView {
                cause: sdk::Cause::Direct,
                dispatch_id,
                recipe_id: "agent/x".into(),
                receiver,
                status: DispatchStatus::Delivered {
                    delivery: sdk::DeliveryOutcome::Applied,
                },
                outcome: Some(Ok(Vec::new())),
                created_at: 0,
                updated_at: 0,
            }),
        )))
    });

    let req = dispatch::encode_query(&dispatch::DispatchQuery::Dispatch {
        receiver: "runs".into(),
        dispatch_id: dispatch_id.into(),
    });
    let bytes = block_on(ctx.query("dispatch", &req)).expect("dispatch served via on_query");
    let dispatch::DispatchReply::Dispatch(Some(view)) =
        dispatch::decode_reply(&bytes).expect("decode reply")
    else {
        panic!("expected a Dispatch view");
    };
    assert_eq!(view.dispatch_id, dispatch_id);
    assert_eq!(view.receiver, "runs");
    assert!(matches!(view.status, DispatchStatus::Delivered { .. }));

    // an unregistered sibling still gets the shared QueryUnsupported default.
    let err = block_on(ctx.query("saga", b"")).unwrap_err();
    assert!(matches!(err, Error::QueryUnsupported));
}
