use super::*;

#[test]
fn task_actions_without_a_configured_tasks_module_fail_the_run() {
    let registry = registry(&["bot"]);
    let mut m = RunsModule::new(
        "runs",
        "chat",
        "saga",
        "attribution",
        "dispatch",
        "agent",
        None,
        None,
    );
    m.models = registry.clone();
    commit(&mut m);
    request_post(&mut m, &registry, 2, &[]);
    commit(&mut m);
    let run_id = run_id_for("general", 2, "bot");

    let mut ctx = CaptureCtx::new()
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(response(&["ok"], vec![create_task("t1", "x")]))),
    )
    .unwrap();
    assert!(ctx.task_msgs().is_empty(), "no task write may escape");
    // the failure still surfaces in chat — the agent holds chat.post.
    assert_eq!(ctx.chat_msgs().len(), 1);
    let breadcrumbs: Vec<String> = ctx
        .events
        .iter()
        .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
        .collect();
    assert!(breadcrumbs.iter().any(|b| b.contains("no tasks module")));
    commit(&mut m);
    assert_eq!(get_pending(&m, &run_id), None);
}

/// the settle lane's per-action duckfs lane (`RunsModule::emit_duckfs_effects`):
/// files' own per-path CAS is the ONE base rule, not a global-head equality —
/// a `base_snapshot: None` action succeeds against a non-empty filesystem as
/// long as its OWN path is new, and a stale write degrades to a breadcrumb
/// instead of failing the whole run (#1664).
#[test]
fn duckfs_write_text_with_no_base_delivers_on_a_non_empty_filesystem() {
    let registry = registry(&["bot"]);
    let mut m = configured(&registry).with_files_module("files");
    request_post(&mut m, &registry, 2, &[]);
    commit(&mut m);
    let run_id = run_id_for("general", 2, "bot");

    // the filesystem has committed history elsewhere (a non-empty global
    // head), but the write's OWN path has never been touched — the deleted
    // global-head check would have refused this on sight; the per-path CAS
    // accepts it.
    let mut ctx = CaptureCtx::new()
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2))
        .with_files_head(&"aa".repeat(32))
        .with_file("/other/unrelated.txt", b"already committed");
    exec(
        &mut m,
        &mut ctx,
        &result_event(
            &run_id,
            Ok(response(
                &["ok"],
                vec![duckfs_write_text(
                    "/shared/agents/qa-fixer/self-improvement/SKILL.md",
                    "lesson",
                    None,
                )],
            )),
        ),
    )
    .unwrap();

    // the reply still lands — a duckfs write is never worth failing the run.
    let posts = ctx.chat_msgs();
    assert_eq!(posts.len(), 1, "the reply must deliver alongside the write");
    let ChatMsg::PostMessage { blocks, .. } = &posts[0] else {
        panic!("expected the run's reply post");
    };
    assert_eq!(*blocks, vec![Block::paragraph("ok")]);

    let files = ctx.files_msgs();
    assert_eq!(files.len(), 1, "the write follow-up must emit");
    match &files[0] {
        FilesMsg::Commit {
            base_snapshot,
            message,
            changes,
        } => {
            assert_eq!(*base_snapshot, None);
            assert_eq!(message, "agent duckfs.write_text");
            assert_eq!(changes.len(), 1);
        }
        other => panic!("expected files commit, got {other:?}"),
    }
    commit(&mut m);
    assert_eq!(get_pending(&m, &run_id), None);
}

/// a write whose base has actually gone stale (the per-path CAS mismatches)
/// degrades alone: no files follow-up escapes, but the reply still delivers —
/// the settle path never fails the whole run over it.
#[test]
fn duckfs_write_text_with_a_stale_base_degrades_without_failing_the_run() {
    let registry = registry(&["bot"]);
    let mut m = configured(&registry).with_files_module("files");
    request_post(&mut m, &registry, 2, &[]);
    commit(&mut m);
    let run_id = run_id_for("general", 2, "bot");

    // base_snapshot: None means "this path must be new" — but the path
    // ALREADY has a committed entry, so the per-path CAS mismatches.
    let path = "/shared/agents/qa-fixer/self-improvement/SKILL.md";
    let mut ctx = CaptureCtx::new()
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2))
        .with_files_head(&"aa".repeat(32))
        .with_file(path, b"already there");
    exec(
        &mut m,
        &mut ctx,
        &result_event(
            &run_id,
            Ok(response(
                &["ok"],
                vec![duckfs_write_text(path, "lesson", None)],
            )),
        ),
    )
    .unwrap();

    assert!(ctx.files_msgs().is_empty(), "a stale write must not escape");
    let posts = ctx.chat_msgs();
    assert_eq!(posts.len(), 1, "the reply must still deliver");
    let ChatMsg::PostMessage { blocks, .. } = &posts[0] else {
        panic!("expected the run's reply post");
    };
    assert_eq!(*blocks, vec![Block::paragraph("ok")]);
    assert!(
        ctx.notes()
            .iter()
            .any(|note| note.contains("base snapshot is stale")),
        "the skip must name stale CAS: {:?}",
        ctx.notes()
    );
    commit(&mut m);
    assert_eq!(get_pending(&m, &run_id), None);
}

#[test]
fn a_squatted_reply_message_id_fails_the_run_instead_of_the_block() {
    let (mut m, registry, run_id) = awaiting_run();
    // someone posted a message whose id IS the run's reply id.
    let mut squatted = transcript(2);
    squatted[1].head.message_id = reply_message_id(&run_id);
    let mut ctx = CaptureCtx::new()
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", squatted);
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(response(&["hi"], vec![]))),
    )
    .unwrap();
    assert!(ctx.msgs.is_empty(), "the squatted id emits NOTHING");
    let breadcrumbs: Vec<String> = ctx
        .events
        .iter()
        .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
        .collect();
    assert!(breadcrumbs.iter().any(|b| b.contains("already taken")));
    commit(&mut m);
    assert_eq!(get_pending(&m, &run_id), None);
}

#[test]
fn a_full_thread_fails_the_run_instead_of_the_block() {
    let registry = registry(&["bot"]);
    let mut m = configured(&registry);
    // the anchor replies to a root that has hit the reply cap.
    let mut root = message(1, "root");
    root.head.reply_count = MAX_THREAD_REPLIES as u64;
    let anchor = message_in("general", 2, Party::Key(vec![1; 32]), "reply", Some(1));
    let full = vec![root, anchor];
    let mut ctx = CaptureCtx::new()
        .at(2)
        .with_program_origin()
        .with_registry(&registry)
        .with_transcript("general", full.clone());
    exec(&mut m, &mut ctx, &engagement("general", 2, vec![])).unwrap();
    commit(&mut m);
    let run_id = run_id_for("general", 2, "bot");

    let mut ctx = CaptureCtx::new()
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", full);
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(response(&["hi"], vec![]))),
    )
    .unwrap();
    assert!(ctx.msgs.is_empty());
    let breadcrumbs: Vec<String> = ctx
        .events
        .iter()
        .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
        .collect();
    assert!(breadcrumbs.iter().any(|b| b.contains("thread reply cap")));
    commit(&mut m);
    assert_eq!(get_pending(&m, &run_id), None);
}

#[test]
fn a_reply_and_a_post_message_into_one_near_full_thread_refuse_the_overflow() {
    // THE SAME-BLOCK ACCOUNTING. the reply and a `chat.post_message` are two
    // posts into ONE thread inside ONE delivery block, and every probe reads
    // COMMITTED state — so both see the same 4095 replies and both pass. chat
    // applies the first (4096) and REJECTS the second, which aborts the delivery
    // block; the mailbox re-injects it and it aborts again, forever. the
    // overflowing post must be refused at validation instead.
    let registry = registry(&["bot"]);
    let mut m = configured(&registry);
    // one reply short of the cap: room for EXACTLY one more post.
    let mut root = message(1, "root");
    root.head.reply_count = MAX_THREAD_REPLIES as u64 - 1;
    let anchor = message_in("general", 2, Party::Key(vec![1; 32]), "reply", Some(1));
    let nearly_full = vec![root, anchor];
    let mut ctx = CaptureCtx::new()
        .at(2)
        .with_program_origin()
        .with_registry(&registry)
        .with_transcript("general", nearly_full.clone());
    exec(&mut m, &mut ctx, &engagement("general", 2, vec![])).unwrap();
    commit(&mut m);
    let run_id = run_id_for("general", 2, "bot");

    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", nearly_full);
    exec(
        &mut m,
        &mut ctx,
        &result_event(
            &run_id,
            Ok(response(
                &["done"],
                vec![post_message("general", "and one more", Some(1))],
            )),
        ),
    )
    .unwrap();

    assert!(
        ctx.notes().iter().any(|n| n.contains("thread reply cap")),
        "the overflow is refused at validation: {:?}",
        ctx.notes()
    );
    // the strict lane's policy: the RUN fails, never the block — and the ⚠
    // reply still fits the one free slot the thread had.
    let posts = ctx.chat_msgs();
    assert_eq!(posts.len(), 1, "only the failure reply: {posts:?}");
    assert!(
        matches!(
            &posts[0],
            ChatMsg::PostMessage { message_id, thread: Some(1), .. }
                if *message_id == reply_message_id(&run_id)
        ),
        "the agent's own post never existed: {:?}",
        posts[0]
    );
    commit(&mut m);
    assert_eq!(recent_runs(&m)[0].outcome, RunOutcome::Failed);
}

#[test]
fn two_post_messages_into_one_near_full_thread_refuse_the_second() {
    // the counter must see the FIRST staged post when it probes the second.
    // the anchor is unthreaded here, so the run's own reply consumes nothing —
    // the two actions alone race for the thread's last free slot.
    let (mut m, registry, run_id) = awaiting_run();
    let mut nearly_full = transcript(2);
    nearly_full[0].head.reply_count = MAX_THREAD_REPLIES as u64 - 1;
    let post = |text: &str| post_message("general", text, Some(1));
    let mut ctx = CaptureCtx::new()
        .at(8)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", nearly_full);
    exec(
        &mut m,
        &mut ctx,
        &result_event(
            &run_id,
            Ok(response(&["done"], vec![post("fits"), post("overflows")])),
        ),
    )
    .unwrap();

    assert!(
        ctx.notes().iter().any(|n| n.contains("thread reply cap")),
        "the second post is refused at validation: {:?}",
        ctx.notes()
    );
    // all-or-nothing: the strict lane fails the whole run, so even the post
    // that WOULD have fit never lands — only the unthreaded ⚠ reply.
    let posts = ctx.chat_msgs();
    assert_eq!(posts.len(), 1, "only the failure reply: {posts:?}");
    commit(&mut m);
    assert_eq!(recent_runs(&m)[0].outcome, RunOutcome::Failed);
}

#[test]
fn a_failed_dispatch_outcome_posts_a_threaded_failure_reply_and_prunes_the_entry() {
    // the anchor is a thread reply, so the failure reply must join the
    // same thread a success reply would have.
    let registry = registry(&["bot"]);
    let mut m = configured(&registry);
    let mut thread_transcript = transcript(1);
    thread_transcript.push(message_in(
        "general",
        2,
        Party::Key(vec![1; 32]),
        "in thread",
        Some(1),
    ));
    let mut ctx = CaptureCtx::new()
        .at(2)
        .with_program_origin()
        .with_registry(&registry)
        .with_transcript("general", thread_transcript.clone());
    exec(&mut m, &mut ctx, &engagement("general", 2, vec![])).unwrap();
    commit(&mut m);
    let run_id = run_id_for("general", 2, "bot");

    // the dispatch plane already folded saga failures, timeouts, and
    // contract violations into the Err lane — one shape lands here. the
    // reason's newlines collapse into the single-paragraph excerpt.
    let mut ctx = CaptureCtx::new()
        .at(20)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", thread_transcript.clone());
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Err("worker exploded\nstack line two".into())),
    )
    .unwrap();
    assert_eq!(
        ctx.chat_msgs(),
        vec![ChatMsg::PostMessage {
            channel_id: "general".into(),
            message_id: reply_message_id(&run_id),
            blocks: vec![
                Block::paragraph("⚠ BOT failed"),
                Block::Code {
                    lang: None,
                    text: "worker exploded\nstack line two".into(),
                },
            ],
            thread: Some(1),
        }],
        "one threaded ⚠ reply with the reason in a code block, authored as the agent"
    );
    commit(&mut m);
    assert_eq!(get_pending(&m, &run_id), None, "the entry pruned");

    // a redelivered result finds no entry: no second post, breadcrumb
    // only — the one-reply-per-run dedup holds.
    let mut ctx = CaptureCtx::new()
        .at(21)
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", thread_transcript);
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Err("worker exploded\nstack line two".into())),
    )
    .unwrap();
    assert!(ctx.msgs.is_empty(), "a redelivery must never double-post");
    let breadcrumbs: Vec<String> = ctx
        .events
        .iter()
        .map(|e| String::from_utf8_lossy(&e.payload).into_owned())
        .collect();
    assert!(breadcrumbs.iter().any(|b| b.contains("unknown dispatch")));
}

#[test]
fn failure_excerpts_are_single_line_and_bounded() {
    assert_eq!(
        failure_excerpt("line one\n\n  line two\tend"),
        "line one line two end"
    );
    assert_eq!(failure_excerpt("  \n \t "), "no error detail");
    let long = "x".repeat(FAILURE_EXCERPT_BYTES * 2);
    let bounded = failure_excerpt(&long);
    assert!(bounded.len() <= FAILURE_EXCERPT_BYTES + '…'.len_utf8());
    assert!(bounded.ends_with('…'));
}

#[test]
fn a_post_into_a_channel_the_agents_account_cannot_post_to_fails_the_run() {
    // the post leaves under the run's program account, so chat's own standing
    // for that account in the TARGET channel decides whether it lands — the
    // validator asks first so the block never carries a post chat refuses.
    let post = |channel: &str| post_message(channel, "hello", None);
    // the capture board answers an account probe as user 9.
    let (mut m, registry, run_id) = awaiting_run();
    let board = |member: u8| {
        CaptureCtx::new()
            .at(8)
            .with_dispatch_origin()
            .with_registry(&registry)
            .with_transcript("general", transcript(2))
            .with_transcript("board", transcript(1))
            .with_members_only("board", vec![member; 32])
    };

    let mut ctx = board(7);
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(response(&["done"], vec![post("board")]))),
    )
    .unwrap();
    assert!(
        ctx.notes()
            .iter()
            .any(|n| n.contains("the agent's account may not post to channel")),
        "{:?}",
        ctx.notes()
    );
    // nothing reaches #board — only the run's own ⚠ failure reply.
    let posts = ctx.chat_msgs();
    assert_eq!(posts.len(), 1, "only the failure reply: {posts:?}");
    commit(&mut m);
    assert_eq!(recent_runs(&m)[0].outcome, RunOutcome::Failed);

    // the same post, by an account that IS a member, lands.
    let (mut m, _registry, run_id) = awaiting_run();
    let mut ctx = board(9);
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(response(&["done"], vec![post("board")]))),
    )
    .unwrap();
    assert_eq!(ctx.chat_msgs().len(), 2, "the reply and the post");
    commit(&mut m);
    assert_eq!(recent_runs(&m)[0].outcome, RunOutcome::ResultAccepted);
}

/// the settle lane's submit lane (`RunsModule::emit_submit_effects`): a final
/// response may carry any module's own message, and it goes out verbatim
/// beside the reply. the module's verdict is its own — runs probes nothing,
/// and the run's outcome is the accepted result either way.
#[test]
fn a_final_response_submits_any_module_message_verbatim_beside_the_reply() {
    let registry = registry(&["bot"]);
    let mut m = configured(&registry);
    request_post(&mut m, &registry, 2, &[]);
    commit(&mut m);
    let run_id = run_id_for("general", 2, "bot");

    let message = serde_json::json!({
        "open_issue": {"repo": "playground", "title": "Flaky gate", "body": ""}
    });
    let submit = envelope(
        crate::OP_SUBMIT,
        Some(serde_json::json!({"module": "forge"})),
        message.clone(),
    );
    let mut ctx = CaptureCtx::new()
        .with_dispatch_origin()
        .with_registry(&registry)
        .with_transcript("general", transcript(2));
    exec(
        &mut m,
        &mut ctx,
        &result_event(&run_id, Ok(response(&["filed"], vec![submit]))),
    )
    .unwrap();

    assert_eq!(
        ctx.chat_msgs().len(),
        1,
        "the reply must deliver beside the submit"
    );
    let to_forge: Vec<&Msg> = ctx.msgs.iter().filter(|m| m.target == "forge").collect();
    assert_eq!(to_forge.len(), 1, "{:?}", ctx.msgs);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&to_forge[0].payload).unwrap(),
        message,
        "the module message is carried verbatim"
    );
    commit(&mut m);
    assert_eq!(get_pending(&m, &run_id), None);
    assert_eq!(recent_runs(&m)[0].outcome, RunOutcome::ResultAccepted);
}
