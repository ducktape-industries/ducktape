use super::*;
use attribution::{Actor, AttributionQuery, AttributionReply, Change, ChangeKind, Reason, Source};
use sdk::Origin;

fn signer(number: u8) -> Origin {
    Origin::External(vec![number; 32])
}
fn discussion_reason() -> Reason {
    Reason::Defined(MANAGED_RECORD_COMMENT_REASON.into())
}
fn source(id: &str) -> Source {
    Source {
        module: "pages".into(),
        kind: "comment".into(),
        object: id.into(),
    }
}

fn block(origin: Origin) -> host::BlockContext {
    host::BlockContext {
        height: 1,
        consensus_time: 7,
        origin,
    }
}

async fn send(host: &mut host::Host, origin: Origin, message: PageMsg) -> host::BlockOutcome {
    host.submit_at(block(origin), msg(&message)).await.unwrap()
}

async fn seed(host: &mut host::Host) {
    for number in 1..=3 {
        host.submit_at(
            block(signer(number)),
            Msg {
                target: "identity".into(),
                payload: identity::encode_msg(&identity::IdentityMsg::Create {
                    name: format!("member-{number}"),
                    scheme: identity::KeyScheme::Ed25519,
                }),
            },
        )
        .await
        .unwrap();
    }
    send(
        host,
        Origin::Program(1),
        PageMsg::CreatePage {
            page_id: "board".into(),
            title: "Managed board".into(),
            blocks: vec![],
        },
    )
    .await;
    send(
        host,
        Origin::Program(1),
        PageMsg::CreateRecordCollection {
            page_id: "board".into(),
            request_id: "create".into(),
        },
    )
    .await;
    send(
        host,
        Origin::Program(1),
        super::records::commit(
            "first",
            0,
            vec![super::records::upsert("ask", "A visible question")],
        ),
    )
    .await;
    send(
        host,
        signer(2),
        PageMsg::CreatePage {
            page_id: "plain".into(),
            title: "Ordinary".into(),
            blocks: vec![para("plain-body", "Ordinary text")],
        },
    )
    .await;
}

fn comment(id: &str, thread: &str, target: &str, text: &str, mentions: Vec<u64>) -> PageMsg {
    PageMsg::AddComment {
        comment_id: id.into(),
        thread_id: thread.into(),
        target: target.into(),
        text: text.into(),
        anchor: None,
        mentions,
    }
}

async fn changes(host: &host::Host, query: AttributionQuery) -> Vec<Change> {
    let bytes = host
        .query("attribution", &attribution::encode_query(&query))
        .await
        .unwrap();
    let AttributionReply::Changes(entries) = attribution::decode_reply(&bytes).unwrap() else {
        panic!("changes")
    };
    entries.into_iter().map(|entry| entry.change).collect()
}

async fn writer_discussion(host: &host::Host) -> Vec<Change> {
    changes(
        host,
        AttributionQuery::ChangesFor {
            recipient: 1,
            after: 0,
            limit: 128,
        },
    )
    .await
    .into_iter()
    .filter(|change| change.reason == discussion_reason())
    .collect()
}

async fn relations(host: &host::Host, id: &str) -> attribution::ObjectRelations {
    let bytes = host
        .query(
            "attribution",
            &attribution::encode_query(&AttributionQuery::Relations { source: source(id) }),
        )
        .await
        .unwrap();
    let AttributionReply::Relations(Some(relations)) = attribution::decode_reply(&bytes).unwrap()
    else {
        panic!("relations")
    };
    relations
}

async fn canonical_comment(host: &host::Host, id: &str) -> Option<Comment> {
    let bytes = host
        .query(
            "pages",
            &encode_query(&PageQuery::GetComment {
                comment_id: id.into(),
            }),
        )
        .await
        .unwrap();
    let PageReply::Comment(comment) = decode_reply(&bytes).unwrap() else {
        panic!("comment")
    };
    comment
}

fn comment_report(outcome: &host::BlockOutcome, id: &str) -> attribution::AttributionUpdate {
    outcome
        .dispatches
        .iter()
        .filter(|dispatch| dispatch.module == "attribution")
        .flat_map(
            |dispatch| match attribution::decode_msg(&dispatch.payload).unwrap() {
                attribution::AttributionMsg::AttributeBatch { updates } => updates,
                other => panic!("expected batch, got {other:?}"),
            },
        )
        .find(|update| update.object.kind == "comment" && update.object.object == id)
        .expect("comment source report")
}

macro_rules! discussion_host {
    ($context:expr) => {
        host::Host::genesis(vec![
            Box::new(
                pages_on!($context, "pages")
                    .with_identity("identity")
                    .with_attribution("attribution"),
            ),
            Box::new(identity::Identity::new(
                "identity",
                Box::new(sdk_testkit::MemStore::new()),
                "discussion".into(),
            )),
            Box::new(attribution::AttributionModule::new(
                "attribution",
                Box::new(sdk_testkit::MemStore::new()),
            )),
        ])
        .unwrap()
    };
}

#[test]
fn plain_member_discussion_reaches_managed_writer_without_rewriting_sender() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = discussion_host!(context);
        seed(&mut host).await;
        let outcome = send(
            &mut host,
            signer(2),
            comment("member-answer", "thread", "ask", "My answer", vec![]),
        )
        .await;
        let events = writer_discussion(&host).await;
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.source, source("member-answer"));
        assert_eq!(event.kind, ChangeKind::Added);
        assert_eq!(event.actor, Actor::Account(2));
        let snapshot: ManagedDiscussionSnapshot = sdk::wire::decode(&event.detail).unwrap();
        assert_eq!(snapshot.mutation, DiscussionMutation::Created);
        assert_eq!(snapshot.collection_page_id, "board");
        assert_eq!(snapshot.page_id, "ask");
        assert_eq!(snapshot.comment.author, Party::Account(2));
        assert_eq!(snapshot.comment.text, "My answer");
        assert_eq!(snapshot.thread.target, "ask");
        let canonical = canonical_comment(&host, "member-answer").await.unwrap();
        assert_eq!(canonical.author, Party::Account(2));
        assert_eq!(canonical.text, "My answer");
        assert!(canonical.mentions.is_empty());
        let report = comment_report(&outcome, "member-answer");
        assert_eq!(report.actor, Actor::Account(2));
        assert!(
            report
                .relations
                .iter()
                .any(|relation| relation.recipient == 2 && relation.reason == Reason::Authorship)
        );
        assert!(
            !report
                .relations
                .iter()
                .any(|relation| relation.reason == Reason::Mention)
        );
        // The same generic relation covers record body and collection-root discussion.
        send(
            &mut host,
            signer(2),
            comment(
                "body-answer",
                "body-thread",
                "ask-body",
                "Body answer",
                vec![],
            ),
        )
        .await;
        send(
            &mut host,
            signer(2),
            comment(
                "board-answer",
                "board-thread",
                "board",
                "Board answer",
                vec![],
            ),
        )
        .await;
        assert_eq!(writer_discussion(&host).await.len(), 3);
    });
}

#[test]
fn managed_snapshot_bound_rejects_atomically_without_changing_plain_comment_limits() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = discussion_host!(context);
        seed(&mut host).await;
        // Raw text stays inside the ordinary text cap; JSON escaping plus an
        // explicitly bounded stored mention list makes the frozen snapshot too large.
        let text = "\u{1}".repeat(MAX_COMMENT_TEXT_BYTES);
        let mentions = vec![3; 80_000];
        let before = host.module_root("pages");
        let failed = host
            .submit_at(
                block(signer(2)),
                msg(&comment(
                    "large-managed",
                    "large-thread",
                    "ask",
                    &text,
                    mentions.clone(),
                )),
            )
            .await;
        assert!(failed.is_err());
        assert_eq!(host.module_root("pages"), before);
        assert!(canonical_comment(&host, "large-managed").await.is_none());
        assert!(writer_discussion(&host).await.is_empty());
        send(
            &mut host,
            signer(2),
            comment(
                "large-plain",
                "plain-large-thread",
                "plain",
                &text,
                mentions,
            ),
        )
        .await;
        assert_eq!(
            canonical_comment(&host, "large-plain").await.unwrap().text,
            text
        );
        assert!(writer_discussion(&host).await.is_empty());
    });
}

#[test]
fn full_json_source_envelope_is_bounded_and_large_valid_snapshots_reach_attribution() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = discussion_host!(context);
        seed(&mut host).await;
        // This raw snapshot is below 512 KiB, but its detail-as-integer-array
        // source/update envelope exceeds the store framing budget.
        let text = "\u{1}".repeat(MAX_COMMENT_TEXT_BYTES);
        let root = host.module_root("pages");
        let error = host
            .submit_at(
                block(signer(2)),
                msg(&comment(
                    "oversized-wire",
                    "oversized-thread",
                    "ask",
                    &text,
                    vec![],
                )),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("attribution source envelope too large"),
            "{error}"
        );
        assert_eq!(host.module_root("pages"), root);
        assert!(canonical_comment(&host, "oversized-wire").await.is_none());
        let text = "\u{1}".repeat(40_000);
        let outcome = send(
            &mut host,
            signer(2),
            comment("large-valid", "large-valid-thread", "ask", &text, vec![]),
        )
        .await;
        let source = relations(&host, "large-valid").await;
        let bytes = sdk::wire::encode(&source);
        assert!(bytes.len() > sdk::MAX_STORE_VALUE_BYTES / 2);
        assert!(bytes.len() <= MAX_BLOCK_LEN);
        for dispatch in outcome
            .dispatches
            .iter()
            .filter(|dispatch| dispatch.module == "attribution")
        {
            assert!(dispatch.payload.len() <= MAX_BLOCK_LEN);
        }
        let event = writer_discussion(&host).await.pop().unwrap();
        let snapshot: ManagedDiscussionSnapshot = sdk::wire::decode(&event.detail).unwrap();
        assert_eq!(snapshot.mutation, DiscussionMutation::Created);
        assert_eq!(snapshot.comment.text, text);
        assert!(
            sdk::wire::encode(&attribution::AttributionEvent::Changed(event)).len()
                <= sdk::MAX_STORE_VALUE_BYTES
        );
    });
}

#[test]
fn reusing_a_deleted_comment_identity_is_not_a_fresh_decision() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = discussion_host!(context);
        seed(&mut host).await;
        send(
            &mut host,
            signer(2),
            comment("answer", "old-thread", "ask", "Original", vec![]),
        )
        .await;
        send(
            &mut host,
            signer(2),
            PageMsg::DeleteComment {
                comment_id: "answer".into(),
            },
        )
        .await;
        assert!(canonical_comment(&host, "answer").await.is_none());
        send(
            &mut host,
            signer(2),
            comment("answer", "new-thread", "ask", "Recycled identity", vec![]),
        )
        .await;
        let events = writer_discussion(&host).await;
        let first: ManagedDiscussionSnapshot = sdk::wire::decode(&events[0].detail).unwrap();
        let reused: ManagedDiscussionSnapshot = sdk::wire::decode(&events[2].detail).unwrap();
        assert_eq!(first.mutation, DiscussionMutation::Created);
        assert_eq!(reused.mutation, DiscussionMutation::Recreated);
        assert_eq!(reused.comment.author, Party::Account(2));
        assert_eq!(events[2].actor, Actor::Account(2));
    });
}

#[test]
fn self_discussion_keeps_truthful_actor_for_downstream_self_suppression() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = discussion_host!(context);
        seed(&mut host).await;
        send(
            &mut host,
            Origin::Program(1),
            comment("self", "thread", "ask", "Program follow-up", vec![]),
        )
        .await;
        let events = writer_discussion(&host).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor, Actor::Account(events[0].recipient));
        assert_eq!(
            canonical_comment(&host, "self").await.unwrap().author,
            Party::Account(1)
        );
        assert!(
            events.iter().all(|event| event.actor == Actor::Account(1)),
            "never fabricate a human sender to wake the writer"
        );
    });
}

#[test]
fn discussion_edits_keep_original_author_but_report_actual_mutator_and_new_content() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = discussion_host!(context);
        seed(&mut host).await;
        send(
            &mut host,
            signer(2),
            comment("answer", "thread", "ask", "Original answer", vec![3, 3]),
        )
        .await;
        let before = relations(&host, "answer").await;
        let before_detail = before
            .relations
            .iter()
            .find(|relation| relation.reason == discussion_reason())
            .unwrap()
            .detail
            .clone();
        let outcome = send(
            &mut host,
            signer(3),
            PageMsg::EditComment {
                comment_id: "answer".into(),
                text: "Edited by a different member".into(),
                mentions: vec![3],
            },
        )
        .await;
        let current = canonical_comment(&host, "answer").await.unwrap();
        assert_eq!(current.author, Party::Account(2));
        assert_eq!(current.text, "Edited by a different member");
        let report = comment_report(&outcome, "answer");
        assert_eq!(report.actor, Actor::Account(3));
        let relation = report
            .relations
            .iter()
            .find(|relation| relation.reason == discussion_reason())
            .unwrap();
        assert!(relation.detail.len() <= MAX_MANAGED_DISCUSSION_BYTES);
        let snapshot: ManagedDiscussionSnapshot = sdk::wire::decode(&relation.detail).unwrap();
        assert_eq!(snapshot.comment.author, Party::Account(2));
        assert_eq!(snapshot.comment.text, current.text);
        assert_eq!(snapshot.mutation, DiscussionMutation::Edited);
        assert_ne!(relation.detail, before_detail);
        assert_eq!(
            report
                .relations
                .iter()
                .filter(|relation| relation.reason == Reason::Mention)
                .count(),
            1
        );
        assert!(
            report
                .relations
                .iter()
                .any(|relation| relation.reason == Reason::Authorship && relation.recipient == 2)
        );
        let after = relations(&host, "answer").await;
        assert!(after.revision > before.revision);
        assert_eq!(
            after
                .relations
                .iter()
                .find(|relation| relation.reason == discussion_reason())
                .unwrap()
                .detail,
            relation.detail
        );
        // Old event content is frozen: fetching the current source cannot pair
        // the new editor's text with the original member's old actor.
        let delivered = writer_discussion(&host).await;
        assert_eq!(
            delivered.len(),
            1,
            "detail-only edits never become fresh approvals; a new reply is required"
        );
        let original = &delivered[0];
        let snapshot: ManagedDiscussionSnapshot = sdk::wire::decode(&original.detail).unwrap();
        assert_eq!(original.actor, Actor::Account(2));
        assert_eq!(snapshot.comment.text, "Original answer");
        // An identical edit at the same consensus time preserves the snapshot.
        let duplicate = send(
            &mut host,
            signer(3),
            PageMsg::EditComment {
                comment_id: "answer".into(),
                text: current.text,
                mentions: vec![3],
            },
        )
        .await;
        assert_eq!(
            comment_report(&duplicate, "answer").relations,
            report.relations
        );
        let events = changes(
            &host,
            AttributionQuery::ChangesOf {
                source: source("answer"),
                after: 0,
                limit: 128,
            },
        )
        .await;
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].seq < pair[1].seq && pair[0].revision <= pair[1].revision)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.reason == Reason::Authorship)
                .count(),
            1,
            "editing never republishes authorship as a fresh human answer"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.reason == Reason::Mention)
                .count(),
            1
        );
    });
}

#[test]
fn duplicate_and_deleted_comments_keep_revisions_ordered_and_withdraw_managed_discussion() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = discussion_host!(context);
        seed(&mut host).await;
        let original = comment("answer", "thread", "ask", "Original answer", vec![3]);
        send(&mut host, signer(2), original.clone()).await;
        let before = host.module_root("attribution");
        assert!(
            host.submit_at(block(signer(3)), msg(&original))
                .await
                .is_err()
        );
        assert_eq!(host.module_root("attribution"), before);
        send(
            &mut host,
            signer(2),
            comment("reply", "thread", "ask", "Keep thread alive", vec![]),
        )
        .await;
        send(
            &mut host,
            signer(3),
            PageMsg::DeleteComment {
                comment_id: "answer".into(),
            },
        )
        .await;
        let tombstone = canonical_comment(&host, "answer").await.unwrap();
        assert!(tombstone.deleted);
        assert_eq!(tombstone.author, Party::Account(2));
        let events: Vec<_> = writer_discussion(&host)
            .await
            .into_iter()
            .filter(|event| event.source == source("answer"))
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, ChangeKind::Withdrawn);
        assert_eq!(events[1].actor, Actor::Account(3));
        assert!(events[1].revision > events[0].revision);
        assert!(relations(&host, "answer").await.relations.is_empty());
        let before = host.module_root("attribution");
        send(
            &mut host,
            signer(3),
            PageMsg::DeleteComment {
                comment_id: "answer".into(),
            },
        )
        .await;
        assert_eq!(host.module_root("attribution"), before);
        // The final live comment deletes its thread and source bytes together;
        // the prior-snapshot resolver still knows which writer to withdraw.
        send(
            &mut host,
            signer(3),
            PageMsg::DeleteComment {
                comment_id: "reply".into(),
            },
        )
        .await;
        assert!(canonical_comment(&host, "reply").await.is_none());
        assert!(relations(&host, "reply").await.relations.is_empty());
    });
}

#[test]
fn thread_retargeting_updates_managed_context_and_plain_pages_keep_existing_relations() {
    deterministic::Runner::default().start(|context| async move {
        let mut host = discussion_host!(context);
        seed(&mut host).await;
        send(
            &mut host,
            signer(2),
            comment(
                "plain-comment",
                "plain-thread",
                "plain",
                "Ordinary",
                vec![3],
            ),
        )
        .await;
        let plain = relations(&host, "plain-comment").await;
        assert_eq!(plain.relations.len(), 2);
        assert!(writer_discussion(&host).await.is_empty());
        let before = host.module_root("attribution");
        send(
            &mut host,
            signer(3),
            PageMsg::MoveCommentThread {
                thread_id: "plain-thread".into(),
                target: "plain-body".into(),
                anchor: None,
            },
        )
        .await;
        assert_eq!(
            host.module_root("attribution"),
            before,
            "unmanaged retargeting still emits no attribution update"
        );
        send(
            &mut host,
            signer(3),
            PageMsg::MoveCommentThread {
                thread_id: "plain-thread".into(),
                target: "ask".into(),
                anchor: None,
            },
        )
        .await;
        let added = writer_discussion(&host).await;
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].actor, Actor::Account(3));
        assert_eq!(added[0].kind, ChangeKind::Added);
        let snapshot: ManagedDiscussionSnapshot = sdk::wire::decode(&added[0].detail).unwrap();
        assert_eq!(snapshot.mutation, DiscussionMutation::Retargeted);
        let before_resolve = writer_discussion(&host).await;
        send(
            &mut host,
            signer(3),
            PageMsg::ResolveThread {
                thread_id: "plain-thread".into(),
                resolved: true,
            },
        )
        .await;
        assert_eq!(writer_discussion(&host).await, before_resolve);
        assert_eq!(
            canonical_comment(&host, "plain-comment")
                .await
                .unwrap()
                .author,
            Party::Account(2)
        );
        send(
            &mut host,
            signer(3),
            PageMsg::MoveCommentThread {
                thread_id: "plain-thread".into(),
                target: "plain".into(),
                anchor: None,
            },
        )
        .await;
        let events = writer_discussion(&host).await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, ChangeKind::Withdrawn);
        assert_eq!(
            relations(&host, "plain-comment").await.relations,
            plain.relations
        );
    });
}
