use super::*;
use sdk::Origin;
use serde_json::json;

fn owner() -> Origin {
    Origin::Program(42)
}

pub(super) fn upsert(id: &str, text: &str) -> RecordChange {
    RecordChange::Upsert {
        record_id: id.into(),
        data: json!({"value": text}),
        document: RecordDocument {
            title: format!("Record {id}"),
            blocks: vec![para(&format!("{id}-body"), text)],
        },
    }
}

pub(super) fn commit(request: &str, expected_revision: u64, changes: Vec<RecordChange>) -> PageMsg {
    PageMsg::CommitRecords {
        page_id: "board".into(),
        expected_revision,
        request_id: request.into(),
        changes,
        state_changes: Vec::new(),
        metadata: None,
        artifacts: Vec::new(),
    }
}

async fn apply(p: &mut Pages, message: &PageMsg) -> RecordReceipt {
    let mut ctx = ctx_as(owner());
    p.execute(&mut ctx, &msg(message)).await.unwrap();
    let receipt = sdk::wire::decode(ctx.output().expect("record receipt output")).unwrap();
    p.commit_block().await.unwrap();
    receipt
}

async fn seed(p: &mut Pages) {
    apply_commit_as(
        p,
        &PageMsg::CreatePage {
            page_id: "board".into(),
            title: "Board".into(),
            blocks: vec![para("intro", "Ordinary visible introduction")],
        },
        owner(),
    )
    .await;
    let receipt = apply(
        p,
        &PageMsg::CreateRecordCollection {
            page_id: "board".into(),
            request_id: "create".into(),
        },
    )
    .await;
    assert_eq!(receipt.revision, 0);
}

async fn query(p: &Pages, query: PageQuery) -> PageReply {
    decode_reply(&p.query(&encode_query(&query)).await.unwrap()).unwrap()
}

async fn rejected(p: &mut Pages, message: &PageMsg, actor: Origin, needle: &str) {
    let root = p.root();
    let before = p.staged.checkpoint();
    let error = p
        .execute(&mut ctx_as(actor), &msg(message))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains(needle),
        "unexpected error: {error}"
    );
    assert_eq!(
        p.staged.checkpoint(),
        before,
        "rejection must restore even uncommitted prior writes"
    );
    assert_eq!(p.root(), root);
}

#[test]
fn records_are_atomic_ordinary_documents_with_revisioned_pagination() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        seed(&mut p).await;
        let root = p.root();
        let receipt = apply(
            &mut p,
            &commit("first", 0, vec![upsert("z", "Z"), upsert("a", "A")]),
        )
        .await;
        assert_eq!(receipt.revision, 1);
        assert_ne!(p.root(), root);
        let PageReply::RecordCollection(Some(collection)) = query(
            &p,
            PageQuery::RecordCollection {
                page_id: "board".into(),
            },
        )
        .await
        else {
            panic!("collection")
        };
        assert_eq!(
            collection,
            RecordCollection {
                page_id: "board".into(),
                writer: Party::Account(42),
                revision: 1,
                record_count: 2
            }
        );
        let PageReply::Records(Some(first)) = query(
            &p,
            PageQuery::Records {
                page_id: "board".into(),
                after: None,
                limit: 1,
            },
        )
        .await
        else {
            panic!("records")
        };
        assert_eq!(first.revision, 1);
        assert_eq!(first.records[0].record_id, "a");
        assert_eq!(first.next_after.as_deref(), Some("a"));
        let PageReply::Records(Some(second)) = query(
            &p,
            PageQuery::Records {
                page_id: "board".into(),
                after: first.next_after,
                limit: 0,
            },
        )
        .await
        else {
            panic!("records")
        };
        assert_eq!(second.records[0].record_id, "z");
        assert_eq!(second.next_after, None);
        let block = get_block(&p, "a").await.unwrap();
        assert_eq!(block.kind, BlockKind::Page);
        assert_eq!(block.parent.as_deref(), Some("board"));
        assert_eq!(block.author, Party::Account(42));
        assert_eq!(get_page(&p, "a").await.unwrap()[1].text, "A");
        assert_eq!(
            get_block(&p, "intro").await.unwrap().text,
            "Ordinary visible introduction"
        );
        assert_eq!(
            query(
                &p,
                PageQuery::Record {
                    page_id: "other".into(),
                    record_id: "a".into()
                }
            )
            .await,
            PageReply::Record(None)
        );
        assert_eq!(
            query(
                &p,
                PageQuery::Records {
                    page_id: "missing".into(),
                    after: None,
                    limit: 0
                }
            )
            .await,
            PageReply::Records(None)
        );
        assert_eq!(
            query(
                &p,
                PageQuery::RecordReceipt {
                    page_id: "board".into(),
                    request_id: "first".into()
                }
            )
            .await,
            PageReply::RecordReceipt(Some(receipt))
        );
    });
}

#[test]
fn full_batches_and_query_caps_work_and_data_only_changes_move_the_root() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        seed(&mut p).await;
        for revision in 0..3 {
            let count = match revision {
                2 => 1,
                _ => MAX_RECORD_CHANGES,
            };
            let changes = (0..count)
                .map(|i| {
                    upsert(
                        &format!("r{:02}", revision as usize * MAX_RECORD_CHANGES + i),
                        "visible",
                    )
                })
                .collect();
            apply(
                &mut p,
                &commit(&format!("batch-{revision}"), revision, changes),
            )
            .await;
        }
        let PageReply::Records(Some(first)) = query(
            &p,
            PageQuery::Records {
                page_id: "board".into(),
                after: None,
                limit: u16::MAX,
            },
        )
        .await
        else {
            panic!("records")
        };
        assert_eq!(first.records.len(), MAX_RECORD_QUERY_LIMIT as usize);
        assert_eq!(first.next_after.as_deref(), Some("r31"));
        let PageReply::Records(Some(last)) = query(
            &p,
            PageQuery::Records {
                page_id: "board".into(),
                after: first.next_after,
                limit: 0,
            },
        )
        .await
        else {
            panic!("records")
        };
        assert_eq!(last.records.len(), 1);
        assert_eq!(last.next_after, None);
        let before = get_page(&p, "r00").await;
        let root = p.root();
        let mut change = upsert("r00", "visible");
        let RecordChange::Upsert { data, .. } = &mut change else {
            unreachable!()
        };
        *data = json!("x".repeat(MAX_RECORD_DATA_BYTES - 2)); // exact serialized data limit
        apply(&mut p, &commit("data-only", 3, vec![change])).await;
        assert_ne!(p.root(), root);
        assert_eq!(
            get_page(&p, "r00").await,
            before,
            "ordinary document is unchanged"
        );
        assert_eq!(p.record("board", "r00").await.unwrap().unwrap().revision, 4);
    });
}

#[test]
fn exact_byte_replays_precede_cas_and_never_rewind_a_newer_document() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages").with_attribution("attribution");
        seed(&mut p).await;
        let first = commit("first", 0, vec![upsert("a", "old")]);
        let receipt = apply(&mut p, &first).await;
        apply(&mut p, &commit("second", 1, vec![upsert("a", "new")])).await;
        let root = p.root();
        let mut ctx = ctx_as(owner());
        p.execute(&mut ctx, &msg(&first)).await.unwrap();
        assert_eq!(ctx.output().unwrap(), sdk::wire::encode(&receipt));
        assert!(ctx.msgs().is_empty(), "replay must not repeat attribution");
        p.commit_block().await.unwrap();
        assert_eq!(p.root(), root);
        assert_eq!(get_block(&p, "a-body").await.unwrap().text, "new");
        let created = apply(
            &mut p,
            &PageMsg::CreateRecordCollection {
                page_id: "board".into(),
                request_id: "create".into(),
            },
        )
        .await;
        assert_eq!(
            created.revision, 0,
            "create replay returns its original receipt"
        );
        assert_eq!(p.root(), root);
        rejected(
            &mut p,
            &commit("first", 2, vec![upsert("a", "different")]),
            owner(),
            "different payload",
        )
        .await;
        rejected(&mut p, &first, Origin::Program(7), "not authorized").await;
        let mut different_bytes = msg(&first);
        different_bytes.payload.push(b' '); // same decoded request, different authenticated bytes
        let error = p
            .execute(&mut ctx_as(owner()), &different_bytes)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("different payload"));
        assert_eq!(p.root(), root);
    });
}

#[test]
fn cas_and_late_batch_failure_preserve_prior_staged_records_and_all_metadata() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages").with_attribution("attribution");
        seed(&mut p).await;
        p.execute(
            &mut ctx_as(owner()),
            &msg(&commit("staged", 0, vec![upsert("a", "staged")])),
        )
        .await
        .unwrap();
        rejected(
            &mut p,
            &commit("stale", 0, vec![upsert("a", "stale")]),
            owner(),
            "revision conflict",
        )
        .await;
        rejected(
            &mut p,
            &commit(
                "late-failure",
                1,
                vec![
                    upsert("a", "must roll back"),
                    RecordChange::Delete {
                        record_id: "missing".into(),
                    },
                ],
            ),
            owner(),
            "record not found",
        )
        .await;
        assert_eq!(get_block(&p, "a-body").await.unwrap().text, "staged");
        assert_eq!(
            p.record_collection("board")
                .await
                .unwrap()
                .unwrap()
                .revision,
            1
        );
        assert!(
            p.record_receipt("board", "late-failure")
                .await
                .unwrap()
                .is_none()
        );
        p.commit_block().await.unwrap();
        let root = p.root();
        p.execute(
            &mut ctx_as(owner()),
            &msg(&commit(
                "abort",
                1,
                vec![RecordChange::Delete {
                    record_id: "a".into(),
                }],
            )),
        )
        .await
        .unwrap();
        assert!(p.record("board", "a").await.unwrap().is_none());
        assert!(get_block(&p, "a").await.is_none());
        p.abort_block().await.unwrap();
        assert_eq!(p.root(), root);
        assert!(p.record("board", "a").await.unwrap().is_some());
        assert!(get_block(&p, "a").await.is_some());
        assert!(p.record_receipt("board", "abort").await.unwrap().is_none());
    });
}

#[test]
fn ordinary_block_mutations_cannot_bypass_managed_data_even_for_the_writer() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        seed(&mut p).await;
        apply(&mut p, &commit("first", 0, vec![upsert("a", "protected")])).await;
        apply_commit_as(
            &mut p,
            &PageMsg::CreatePage {
                page_id: "outside".into(),
                title: "Outside".into(),
                blocks: vec![para("outside-body", "plain")],
            },
            owner(),
        )
        .await;
        for id in ["board", "intro", "a", "a-body"] {
            let ops = vec![
                PageMsg::UpdateText {
                    block_id: id.into(),
                    text: "bypass".into(),
                    marks: None,
                },
                PageMsg::SetKind {
                    block_id: id.into(),
                    kind: BlockKind::Paragraph,
                },
                PageMsg::SetChecked {
                    block_id: id.into(),
                    checked: true,
                },
                PageMsg::SetSpanMark {
                    block_id: id.into(),
                    start: 0,
                    end: 1,
                    kind: InlineMark::Bold,
                    active: true,
                },
                PageMsg::RemoveBlock {
                    block_id: id.into(),
                },
                PageMsg::MoveBlock {
                    block_id: id.into(),
                    parent: Some("outside".into()),
                    after: None,
                },
                PageMsg::MoveBlock {
                    block_id: id.into(),
                    parent: None,
                    after: None,
                },
                PageMsg::InsertBlock {
                    parent: id.into(),
                    after: None,
                    block: para("injected", "bypass"),
                },
                PageMsg::MoveBlock {
                    block_id: "outside-body".into(),
                    parent: Some(id.into()),
                    after: None,
                },
            ];
            for op in ops {
                rejected(&mut p, &op, owner(), "requires commit_records").await;
            }
        }
        // Ordinary Pages elsewhere remain collaborative.
        apply_commit_as(
            &mut p,
            &PageMsg::UpdateText {
                block_id: "outside-body".into(),
                text: "collaborative".into(),
                marks: None,
            },
            Origin::Program(7),
        )
        .await;
        rejected(
            &mut p,
            &commit("intruder", 1, vec![upsert("a", "forged")]),
            Origin::Program(7),
            "not authorized",
        )
        .await;
        assert_eq!(get_block(&p, "a-body").await.unwrap().text, "protected");
    });
}

#[test]
fn attachment_is_authorized_and_cannot_capture_a_nested_or_foreign_page() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        apply_commit_as(&mut p, &PageMsg::CreatePage { page_id: "board".into(), title: "Board".into(), blocks: vec![page("nested", "Nested")] }, owner()).await;
        let create = PageMsg::CreateRecordCollection { page_id: "board".into(), request_id: "create".into() };
        rejected(&mut p, &create, Origin::Program(7), "not authorized").await;
        rejected(&mut p, &create, owner(), "top-level page").await;
        rejected(&mut p, &PageMsg::CreateRecordCollection { page_id: "nested".into(), request_id: "create".into() }, owner(), "top-level page").await;
        apply_commit_as(&mut p, &PageMsg::RemoveBlock { block_id: "nested".into() }, owner()).await;
        apply(&mut p, &create).await;
        rejected(&mut p, &PageMsg::CreateRecordCollection { page_id: "board".into(), request_id: "other".into() }, owner(), "already exists").await;
        // Page.author is not a payload field and neither is the writer.
        assert!(decode_msg(br#"{"create_record_collection":{"page_id":"board","request_id":"x","writer":{"account":7}}}"#).is_err());
    });
}

#[test]
fn bounded_batches_reject_duplicate_ids_subpages_and_poison_payloads() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        seed(&mut p).await;
        let cases = vec![
            commit("empty", 0, vec![]),
            commit(
                "many",
                0,
                (0..=MAX_RECORD_CHANGES)
                    .map(|i| upsert(&format!("r{i}"), "x"))
                    .collect(),
            ),
            commit("duplicate", 0, vec![upsert("r", "a"), upsert("r", "b")]),
            commit("reserved", 0, vec![upsert("\0record", "x")]),
            commit("empty-id", 0, vec![upsert("", "x")]),
            commit(
                "long-id",
                0,
                vec![upsert(&"x".repeat(MAX_BLOCK_ID_BYTES + 1), "x")],
            ),
            commit(
                "data",
                0,
                vec![RecordChange::Upsert {
                    record_id: "r".into(),
                    data: json!("x".repeat(MAX_RECORD_DATA_BYTES)),
                    document: RecordDocument {
                        title: "r".into(),
                        blocks: vec![],
                    },
                }],
            ),
            commit(
                "body",
                0,
                vec![RecordChange::Upsert {
                    record_id: "r".into(),
                    data: json!({}),
                    document: RecordDocument {
                        title: "r".into(),
                        blocks: (0..=MAX_RECORD_DOCUMENT_BLOCKS)
                            .map(|i| para(&format!("b{i}"), "x"))
                            .collect(),
                    },
                }],
            ),
            commit(
                "subpage",
                0,
                vec![RecordChange::Upsert {
                    record_id: "r".into(),
                    data: json!({}),
                    document: RecordDocument {
                        title: "r".into(),
                        blocks: vec![page("subpage", "no")],
                    },
                }],
            ),
            commit(
                "payload",
                0,
                vec![upsert("r", &"x".repeat(MAX_RECORD_BATCH_BYTES))],
            ),
        ];
        for case in cases {
            rejected(&mut p, &case, owner(), "invalid or oversized record batch").await;
        }
        rejected(
            &mut p,
            &commit(
                "global-collision",
                0,
                vec![upsert("intro", "cannot overwrite ordinary blocks")],
            ),
            owner(),
            "duplicate block id",
        )
        .await;
        assert_eq!(
            p.record_collection("board")
                .await
                .unwrap()
                .unwrap()
                .revision,
            0
        );
    });
}

fn state_commit(
    request: &str,
    revision: u64,
    state_changes: Vec<RecordStateChange>,
    metadata: Option<serde_json::Value>,
) -> PageMsg {
    PageMsg::CommitRecords {
        page_id: "board".into(),
        request_id: request.into(),
        expected_revision: revision,
        changes: vec![],
        state_changes,
        metadata,
        artifacts: Vec::new(),
    }
}

#[test]
fn protected_state_and_immutable_receipt_metadata_share_the_document_cas() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        seed(&mut p).await;
        let mut first = state_commit(
            "first",
            0,
            vec![RecordStateChange::Put {
                key: "outbox/01".into(),
                value: json!({"payload": "first"}),
            }],
            Some(json!({"fingerprint": "input", "result": {"accepted": true}})),
        );
        let PageMsg::CommitRecords { changes, .. } = &mut first else {
            unreachable!()
        };
        changes.push(upsert("visible", "Visible record document"));
        let receipt = apply(&mut p, &first).await;
        assert_eq!(
            receipt.metadata,
            Some(json!({"fingerprint": "input", "result": {"accepted": true}}))
        );
        assert_eq!(
            query(
                &p,
                PageQuery::RecordState {
                    page_id: "board".into(),
                    key: "outbox/01".into()
                }
            )
            .await,
            PageReply::RecordState(Some(RecordState {
                key: "outbox/01".into(),
                value: json!({"payload": "first"}),
                revision: 1
            }))
        );
        assert_eq!(
            query(
                &p,
                PageQuery::RecordState {
                    page_id: "other".into(),
                    key: "outbox/01".into()
                }
            )
            .await,
            PageReply::RecordState(None)
        );
        assert!(
            get_block(&p, "outbox/01").await.is_none(),
            "state never creates hidden document rows"
        );
        assert_eq!(
            query(&p, PageQuery::PageCount).await,
            PageReply::PageCount(2)
        );
        let document = get_page(&p, "visible").await;
        let metadata_only = state_commit(
            "receipt-only",
            1,
            vec![],
            Some(json!({"result": "no document change"})),
        );
        let metadata_receipt = apply(&mut p, &metadata_only).await;
        assert_eq!(metadata_receipt.revision, 2);
        assert!(
            crate::client::delta_from_op(&encode_msg(&metadata_only))
                .unwrap()
                .kind
                .is_empty()
        );
        apply(
            &mut p,
            &state_commit(
                "state-only",
                2,
                vec![RecordStateChange::Put {
                    key: "outbox/01".into(),
                    value: json!({"payload": "new"}),
                }],
                None,
            ),
        )
        .await;
        let root = p.root();
        assert_eq!(apply(&mut p, &first).await, receipt);
        assert_eq!(p.root(), root);
        assert_eq!(
            p.record_state("board", "outbox/01")
                .await
                .unwrap()
                .unwrap()
                .revision,
            3
        );
        assert_eq!(get_page(&p, "visible").await, document);
        let mut conflicting = first.clone();
        let PageMsg::CommitRecords { metadata, .. } = &mut conflicting else {
            unreachable!()
        };
        *metadata = Some(json!({"result": "different"}));
        rejected(&mut p, &conflicting, owner(), "different payload").await;
        rejected(
            &mut p,
            &state_commit(
                "stale",
                1,
                vec![RecordStateChange::Delete {
                    key: "outbox/01".into(),
                }],
                None,
            ),
            owner(),
            "revision conflict",
        )
        .await;
        rejected(
            &mut p,
            &state_commit(
                "unauthorized",
                3,
                vec![RecordStateChange::Delete {
                    key: "outbox/01".into(),
                }],
                None,
            ),
            Origin::Program(7),
            "not authorized",
        )
        .await;
        let mut late = state_commit(
            "late",
            3,
            vec![RecordStateChange::Delete {
                key: "missing".into(),
            }],
            Some(json!({"result": "must not persist"})),
        );
        let PageMsg::CommitRecords { changes, .. } = &mut late else {
            unreachable!()
        };
        changes.push(upsert("ghost", "Must roll back"));
        rejected(&mut p, &late, owner(), "state key not found").await;
        assert!(get_block(&p, "ghost").await.is_none());
        assert!(p.record_receipt("board", "late").await.unwrap().is_none());
        apply(
            &mut p,
            &state_commit(
                "delete",
                3,
                vec![RecordStateChange::Delete {
                    key: "outbox/01".into(),
                }],
                None,
            ),
        )
        .await;
        assert!(
            p.record_state("board", "outbox/01")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            query(&p, PageQuery::PageCount).await,
            PageReply::PageCount(2)
        );
        assert_eq!(
            p.record_receipt("board", "first").await.unwrap().unwrap(),
            receipt
        );
    });
}

#[test]
fn state_and_receipt_bounds_reject_atomically_and_do_not_consume_page_slots() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        seed(&mut p).await;
        let cases = vec![
            state_commit(
                "metadata",
                0,
                vec![],
                Some(json!("x".repeat(MAX_RECORD_METADATA_BYTES))),
            ),
            state_commit(
                "value",
                0,
                vec![RecordStateChange::Put {
                    key: "k".into(),
                    value: json!("x".repeat(MAX_RECORD_STATE_VALUE_BYTES)),
                }],
                None,
            ),
            state_commit(
                "duplicate",
                0,
                vec![
                    RecordStateChange::Put {
                        key: "k".into(),
                        value: json!(1),
                    },
                    RecordStateChange::Delete { key: "k".into() },
                ],
                None,
            ),
            state_commit(
                "reserved",
                0,
                vec![RecordStateChange::Put {
                    key: "\0k".into(),
                    value: json!(1),
                }],
                None,
            ),
            state_commit(
                "empty",
                0,
                vec![RecordStateChange::Put {
                    key: "".into(),
                    value: json!(1),
                }],
                None,
            ),
        ];
        for case in cases {
            rejected(&mut p, &case, owner(), "invalid or oversized record batch").await;
        }
        for revision in 0..(MAX_RECORD_STATE_KEYS / MAX_RECORD_CHANGES) {
            let state_changes = (0..MAX_RECORD_CHANGES)
                .map(|i| RecordStateChange::Put {
                    key: format!("k{:03}", revision * MAX_RECORD_CHANGES + i),
                    value: json!(i),
                })
                .collect();
            apply(
                &mut p,
                &state_commit(
                    &format!("batch-{revision}"),
                    revision as u64,
                    state_changes,
                    None,
                ),
            )
            .await;
        }
        let revision = (MAX_RECORD_STATE_KEYS / MAX_RECORD_CHANGES) as u64;
        rejected(
            &mut p,
            &state_commit(
                "overflow",
                revision,
                vec![RecordStateChange::Put {
                    key: "overflow".into(),
                    value: json!(1),
                }],
                Some(json!({"result": "no"})),
            ),
            owner(),
            "too many record state keys",
        )
        .await;
        assert!(p.record_state("board", "overflow").await.unwrap().is_none());
        assert!(
            p.record_receipt("board", "overflow")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            query(&p, PageQuery::PageCount).await,
            PageReply::PageCount(1)
        );
        apply(
            &mut p,
            &state_commit(
                "exact-bounds",
                revision,
                vec![RecordStateChange::Put {
                    key: "k000".into(),
                    value: json!("x".repeat(MAX_RECORD_STATE_VALUE_BYTES - 2)),
                }],
                Some(json!("x".repeat(MAX_RECORD_METADATA_BYTES - 2))),
            ),
        )
        .await;
        assert_eq!(
            p.record_state("board", "k000")
                .await
                .unwrap()
                .unwrap()
                .revision,
            revision + 1
        );
    });
}

#[test]
fn a_batch_shares_one_comment_fanout_and_removal_budget() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        seed(&mut p).await;
        apply(
            &mut p,
            &commit(
                "first",
                0,
                ["a", "b", "c", "d"]
                    .into_iter()
                    .map(|id| upsert(id, "hello"))
                    .collect(),
            ),
        )
        .await;
        for id in ["a", "b", "c", "d"] {
            let target = format!("{id}-body");
            let first_thread = format!("{id}-thread-0");
            let first_comment = format!("{id}-comment-0");
            apply_commit_as(
                &mut p,
                &PageMsg::AddComment {
                    thread_id: first_thread.clone(),
                    comment_id: first_comment.clone(),
                    target: target.clone(),
                    text: "review".into(),
                    anchor: Some(RelativeAnchor { start: 0, end: 1 }),
                    mentions: vec![],
                },
                Origin::Program(7),
            )
            .await;
            let thread = p.load_thread(&first_thread).await.unwrap().unwrap();
            let comment = p.load_comment(&first_comment).await.unwrap().unwrap();
            let mut threads = vec![first_thread];
            // Seed the same valid per-target state as 900 one-comment threads
            // without paying the append cap's quadratic setup reads.
            for i in 1..900 {
                let mut thread = thread.clone();
                let mut comment = comment.clone();
                thread.id = format!("{id}-thread-{i}");
                comment.id = format!("{id}-comment-{i}");
                comment.thread_id = thread.id.clone();
                thread.comment_ids = vec![comment.id.clone()];
                p.stage(
                    &crate::comment_ops::thread_key(&thread.id),
                    sdk::wire::encode(&thread),
                )
                .unwrap();
                p.stage(
                    &crate::comment_ops::comment_key(&comment.id),
                    sdk::wire::encode(&comment),
                )
                .unwrap();
                threads.push(thread.id);
            }
            p.stage(
                &crate::comment_ops::target_index_key(&target),
                sdk::wire::encode(&threads),
            )
            .unwrap();
            p.commit_block().await.unwrap();
        }
        let changes = ["a", "b", "c", "d"]
            .into_iter()
            .map(|id| upsert(id, "new hello"))
            .collect();
        rejected(
            &mut p,
            &commit("fanout", 1, changes),
            owner(),
            "invalid or oversized record batch",
        )
        .await;
        assert_eq!(
            query_thread(&p, "a-thread-0").await.unwrap().thread.anchor,
            Some(RelativeAnchor { start: 0, end: 1 })
        );
        let deletions = ["a", "b", "c", "d"]
            .into_iter()
            .map(|id| RecordChange::Delete {
                record_id: id.into(),
            })
            .collect();
        rejected(
            &mut p,
            &commit("remove-all", 1, deletions),
            owner(),
            "subtree is too large to remove",
        )
        .await;
        apply(
            &mut p,
            &commit(
                "remove-one",
                1,
                vec![RecordChange::Delete {
                    record_id: "a".into(),
                }],
            ),
        )
        .await;
        assert!(p.record("board", "a").await.unwrap().is_none());
        assert!(p.record("board", "b").await.unwrap().is_some());
    });
}

#[test]
fn managed_updates_preserve_retained_comment_anchors_and_delete_omitted_documents() {
    deterministic::Runner::default().start(|context| async move {
        let mut p = pages_on!(context, "pages");
        seed(&mut p).await;
        apply(
            &mut p,
            &commit("first", 0, vec![upsert("a", "hello world")]),
        )
        .await;
        apply_commit_as(
            &mut p,
            &PageMsg::AddComment {
                thread_id: "thread".into(),
                comment_id: "comment".into(),
                target: "a-body".into(),
                text: "review".into(),
                anchor: Some(RelativeAnchor { start: 6, end: 11 }),
                mentions: vec![],
            },
            Origin::Program(7),
        )
        .await;
        apply(
            &mut p,
            &commit("update", 1, vec![upsert("a", "new hello world")]),
        )
        .await;
        let thread = query_thread(&p, "thread").await.unwrap();
        assert_eq!(
            thread.thread.anchor,
            Some(RelativeAnchor { start: 10, end: 15 })
        );
        assert_eq!(thread.comments[0].author, Party::Account(7));
        assert_eq!(
            get_block(&p, "a-body").await.unwrap().author,
            Party::Account(42)
        );
        let receipt = apply(
            &mut p,
            &commit(
                "delete",
                2,
                vec![RecordChange::Delete {
                    record_id: "a".into(),
                }],
            ),
        )
        .await;
        assert_eq!(receipt.revision, 3);
        assert!(p.record("board", "a").await.unwrap().is_none());
        assert!(get_block(&p, "a").await.is_none());
        assert!(get_block(&p, "a-body").await.is_none());
        assert!(query_thread(&p, "thread").await.is_none());
        assert_eq!(
            p.record_collection("board")
                .await
                .unwrap()
                .unwrap()
                .record_count,
            0
        );
        assert!(p.record_receipt("board", "first").await.unwrap().is_some());
    });
}
