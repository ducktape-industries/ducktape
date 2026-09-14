use super::*;
use commonware_runtime::Supervisor as _;
use sdk::Origin;

macro_rules! artifact_host {
    ($context:expr, $dir:expr) => {{
        let mut files = files::Files::open("files", $dir.path().to_path_buf()).unwrap();
        files.set_history_window_for_tests(1);
        host::Host::genesis(vec![
            Box::new(
                pages_on!($context.child("pages"), "pages")
                    .with_files("files")
                    .with_attribution("attribution")
                    .with_identity("identity"),
            ),
            Box::new(files),
            Box::new(identity::Identity::new(
                "identity",
                Box::new(sdk_testkit::MemStore::new()),
                "retention".into(),
            )),
            Box::new(attribution::AttributionModule::new(
                "attribution",
                Box::new(sdk_testkit::MemStore::new()),
            )),
        ])
        .unwrap()
    }};
}

fn context(height: u64) -> host::BlockContext {
    host::BlockContext {
        height,
        consensus_time: height,
        origin: Origin::External(vec![1; 32]),
    }
}

fn file_msg(message: &files::FilesMsg) -> Msg {
    Msg {
        target: "files".into(),
        payload: files::encode_msg(message),
    }
}

async fn pages_reply(host: &host::Host, query: PageQuery) -> PageReply {
    decode_reply(&host.query("pages", &encode_query(&query)).await.unwrap()).unwrap()
}

async fn files_reply(host: &host::Host, query: files::FilesQuery) -> files::FilesReply {
    files::decode_reply(
        &host
            .query("files", &files::encode_query(&query))
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn prepare(host: &mut host::Host) -> String {
    host.submit_at(
        host::BlockContext {
            height: 0,
            consensus_time: 0,
            origin: Origin::External(vec![1; 32]),
        },
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&identity::IdentityMsg::Create {
                name: "artifact-writer".into(),
                scheme: identity::KeyScheme::Ed25519,
            }),
        },
    )
    .await
    .unwrap();
    host.submit_at(
        context(1),
        file_msg(&files::FilesMsg::Commit {
            base_snapshot: None,
            message: "artifact candidate".into(),
            changes: vec![files::Change::Put {
                path: "/shared/report.txt".into(),
                exec: false,
                meta: BTreeMap::new(),
                content: files::Content::Inline {
                    b64: "cGF5bG9hZA==".into(),
                },
            }],
        }),
    )
    .await
    .unwrap();
    let files::FilesReply::Refs(refs) = files_reply(host, files::FilesQuery::Refs {}).await else {
        panic!("refs")
    };
    let outcome = host
        .submit_at(
            context(2),
            file_msg(&files::FilesMsg::ProjectSnapshot {
                snapshot: refs.head.unwrap(),
                path: "/shared/report.txt".into(),
            }),
        )
        .await
        .unwrap();
    let dispatch = outcome
        .dispatches
        .iter()
        .find(|dispatch| dispatch.module == "files")
        .unwrap();
    let output = files::decode_write_output(dispatch.output.as_deref().unwrap()).unwrap();
    let files::WriteOutcome::ProjectSnapshot { snapshot } = output.outcome else {
        panic!("project snapshot")
    };
    host.submit_at(
        context(3),
        msg(&PageMsg::CreatePage {
            page_id: "board".into(),
            title: "Board".into(),
            blocks: vec![],
        }),
    )
    .await
    .unwrap();
    host.submit_at(
        context(4),
        msg(&PageMsg::CreateRecordCollection {
            page_id: "board".into(),
            request_id: "create".into(),
        }),
    )
    .await
    .unwrap();
    snapshot
}

fn with_artifacts(request: &str, artifacts: Vec<String>) -> PageMsg {
    PageMsg::CommitRecords {
        page_id: "board".into(),
        request_id: request.into(),
        expected_revision: 0,
        changes: vec![super::records::upsert("record", "Human-visible report")],
        state_changes: vec![RecordStateChange::Put {
            key: "outbox".into(),
            value: serde_json::json!({"ready": true}),
        }],
        metadata: Some(serde_json::json!({"fingerprint": "input", "result": {"accepted": true}})),
        artifacts,
    }
}

fn retention_query(request: &str, index: usize) -> files::FilesQuery {
    files::FilesQuery::Retention {
        module_id: "pages".into(),
        key: crate::record_ops::artifact_retention_key("board", request, index),
    }
}

#[test]
fn receipt_artifacts_are_real_module_owned_gc_roots_and_replay_emits_nothing() {
    deterministic::Runner::default().start(|runtime| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = artifact_host!(runtime, dir);
        let candidate = prepare(&mut host).await;
        let message = with_artifacts("accepted", vec![candidate.clone()]);
        let outcome = host.submit_at(context(5), msg(&message)).await.unwrap();
        let retention_dispatch = outcome
            .dispatches
            .iter()
            .position(|dispatch| dispatch.module == "files")
            .unwrap();
        let attribution_dispatch = outcome
            .dispatches
            .iter()
            .position(|dispatch| dispatch.module == "attribution")
            .unwrap();
        assert!(
            retention_dispatch < attribution_dispatch,
            "retention effects precede source reports"
        );
        assert_eq!(
            files_reply(&host, retention_query("accepted", 0)).await,
            files::FilesReply::Retention(Some(files::RetentionReference {
                snapshot: candidate.clone(),
                revision: 1
            }))
        );
        let PageReply::RecordReceipt(Some(receipt)) = pages_reply(
            &host,
            PageQuery::RecordReceipt {
                page_id: "board".into(),
                request_id: "accepted".into(),
            },
        )
        .await
        else {
            panic!("receipt")
        };
        assert_eq!(receipt.artifacts, vec![candidate.clone()]);
        let files_root = host.module_root("files");
        let pages_root = host.module_root("pages");
        let replay = host.submit_at(context(6), msg(&message)).await.unwrap();
        assert_eq!(host.module_root("files"), files_root);
        assert_eq!(host.module_root("pages"), pages_root);
        assert!(
            replay
                .dispatches
                .iter()
                .all(|dispatch| dispatch.module == "pages")
        );
        let changed = with_artifacts("accepted", vec!["0".repeat(64)]);
        assert!(host.submit_at(context(7), msg(&changed)).await.is_err());
        assert_eq!(host.module_root("files"), files_root);
        host.submit_at(
            context(8),
            msg(&PageMsg::CommitRecords {
                page_id: "board".into(),
                request_id: "trim-current".into(),
                expected_revision: 1,
                changes: vec![RecordChange::Delete {
                    record_id: "record".into(),
                }],
                state_changes: vec![RecordStateChange::Delete {
                    key: "outbox".into(),
                }],
                metadata: None,
                artifacts: vec![],
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            files_reply(&host, retention_query("accepted", 0)).await,
            files::FilesReply::Retention(Some(files::RetentionReference {
                snapshot: candidate.clone(),
                revision: 1
            }))
        );
        let files::FilesReply::Refs(refs) = files_reply(&host, files::FilesQuery::Refs {}).await
        else {
            panic!("refs")
        };
        assert!(
            refs.pins.is_empty(),
            "receipt retention is not an ordinary removable pin"
        );
        // Evict the scoped candidate from the one-entry history window and
        // trigger real Files GC after removing its content from the public head.
        host.submit_at(
            context(files::GC_PERIOD_BLOCKS + 1),
            file_msg(&files::FilesMsg::Commit {
                base_snapshot: refs.head,
                message: "trim public state".into(),
                changes: vec![files::Change::Rm {
                    path: "/shared/report.txt".into(),
                }],
            }),
        )
        .await
        .unwrap();
        let files::FilesReply::Read { b64, .. } = files_reply(
            &host,
            files::FilesQuery::Read {
                path: "/shared/report.txt".into(),
                snapshot: Some(candidate.clone()),
                offset: 0,
                len: files::MAX_READ_BYTES,
            },
        )
        .await
        else {
            panic!("retained read")
        };
        assert_eq!(b64, "cGF5bG9hZA==");
        assert_eq!(
            pages_reply(
                &host,
                PageQuery::Record {
                    page_id: "board".into(),
                    record_id: "record".into()
                }
            )
            .await,
            PageReply::Record(None)
        );
        let root = host.module_root("pages").unwrap();
        drop(host);
        let mut reopened = files::Files::open("files", dir.path().to_path_buf()).unwrap();
        reopened.force_gc();
        assert!(
            reopened
                .gc_mark_for_test()
                .contains(&files::from_hex_32(&candidate).unwrap())
        );
        // Receipt artifact associations survive authenticated Pages state sync.
        let source = QmdbStore::init(runtime.child("source"), "pages").await;
        assert_eq!(source.root(), root);
        let target = source.sync_boundary_target().await;
        let store = QmdbStore::sync_from(
            runtime.child("destination"),
            "destination",
            target,
            source.into_resolver(),
        )
        .await
        .unwrap();
        let synced = Pages::new("pages", Box::new(store)).with_files("files");
        assert_eq!(synced.root(), root);
        assert_eq!(
            synced.record_receipt("board", "accepted").await.unwrap(),
            Some(receipt)
        );
    });
}

#[test]
fn a_late_files_rejection_rolls_back_retention_pages_documents_state_and_receipt() {
    deterministic::Runner::default().start(|runtime| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut host = artifact_host!(runtime, dir);
        let candidate = prepare(&mut host).await;
        let roots = host.module_roots();
        let message = with_artifacts("rejected", vec![candidate, "0".repeat(64)]);
        assert!(host.submit_at(context(5), msg(&message)).await.is_err());
        assert_eq!(host.module_roots(), roots);
        assert_eq!(
            files_reply(&host, retention_query("rejected", 0)).await,
            files::FilesReply::Retention(None)
        );
        assert_eq!(
            pages_reply(
                &host,
                PageQuery::RecordReceipt {
                    page_id: "board".into(),
                    request_id: "rejected".into()
                }
            )
            .await,
            PageReply::RecordReceipt(None)
        );
        assert_eq!(
            pages_reply(
                &host,
                PageQuery::RecordState {
                    page_id: "board".into(),
                    key: "outbox".into()
                }
            )
            .await,
            PageReply::RecordState(None)
        );
        assert_eq!(
            pages_reply(
                &host,
                PageQuery::GetBlock {
                    block_id: "record".into()
                }
            )
            .await,
            PageReply::Block(None)
        );
        let PageReply::RecordCollection(Some(collection)) = pages_reply(
            &host,
            PageQuery::RecordCollection {
                page_id: "board".into(),
            },
        )
        .await
        else {
            panic!("collection")
        };
        assert_eq!(collection.revision, 0);
    });
}

#[test]
fn artifact_bounds_and_missing_files_configuration_fail_before_committing() {
    deterministic::Runner::default().start(|runtime| async move {
        let mut pages = pages_on!(runtime, "pages");
        for message in [
            PageMsg::CreatePage {
                page_id: "board".into(),
                title: "Board".into(),
                blocks: vec![],
            },
            PageMsg::CreateRecordCollection {
                page_id: "board".into(),
                request_id: "create".into(),
            },
        ] {
            apply_commit_as(&mut pages, &message, Origin::Program(42)).await;
        }
        apply_err_as(
            &mut pages,
            &with_artifacts("missing-files", vec!["a".repeat(64)]),
            Origin::Program(42),
            "configured files module",
        )
        .await;
        pages = pages.with_files("files");
        for (request, artifacts) in [
            (
                "count",
                (0..=MAX_RECORD_ARTIFACTS)
                    .map(|index| format!("{index:064x}"))
                    .collect(),
            ),
            ("duplicate", vec!["a".repeat(64), "a".repeat(64)]),
            ("invalid", vec!["not-a-snapshot".into()]),
        ] {
            apply_err_as(
                &mut pages,
                &with_artifacts(request, artifacts),
                Origin::Program(42),
                "invalid or oversized record batch",
            )
            .await;
            assert!(
                pages
                    .record_receipt("board", request)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let message = with_artifacts(
            "max",
            (0..MAX_RECORD_ARTIFACTS)
                .map(|index| format!("{index:064x}"))
                .collect(),
        );
        let mut ctx = ctx_as(Origin::Program(42));
        pages.execute(&mut ctx, &msg(&message)).await.unwrap();
        assert_eq!(
            ctx.msgs().len(),
            MAX_RECORD_ARTIFACTS,
            "bounded retention intents; Files validates candidate reachability at host dispatch"
        );
        pages.abort_block().await.unwrap();
    });
}
