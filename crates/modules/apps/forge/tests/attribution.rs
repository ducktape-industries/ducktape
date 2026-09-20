//! Real Host publication, ordinary program rights and durable source history.
use attribution::{Actor, AttributionQuery, AttributionReply, ObjectRelations, Reason, Source};
use attribution_module::AttributionModule;
use chat::Party;
use forge::{Forge, ForgeMsg, ForgeQuery, ForgeReply, RefUpdate, ReviewVerdict};
use futures::executor::block_on;
use host::{BlockContext, Host};
use identity::{IdentityMsg, KeyScheme};
use identity_module::Identity;
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use sdk_testkit::{MemStore, TestCtx};
use std::path::PathBuf;

struct Directory(PathBuf);
impl Directory {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("forge-attribution-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Executor;
#[async_trait::async_trait(?Send)]
impl Module for Executor {
    fn id(&self) -> ModuleId {
        "executor".into()
    }
    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }
    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        identity::authenticate_event(&ctx.env().origin, "identity", &msg.payload)
            .map_err(|e| Error::module("authenticate_event", e))?;
        Ok(())
    }
}
fn key(byte: u8) -> Origin {
    Origin::External(vec![byte; 32])
}
fn message<T: serde::Serialize>(target: &str, input: &T) -> Msg {
    Msg {
        target: target.into(),
        payload: sdk::wire::encode(input),
    }
}
fn context(origin: Origin) -> BlockContext {
    BlockContext {
        height: 1,
        consensus_time: 1,
        origin,
    }
}
async fn apply<T: serde::Serialize>(host: &mut Host, origin: Origin, target: &str, input: T) {
    host.submit_at(context(origin), message(target, &input))
        .await
        .unwrap();
}
async fn boot(base: &Directory) -> Host {
    let mut host = Host::new();
    host.register(Box::new(Identity::new(
        "identity",
        Box::new(MemStore::new()),
        "sources".into(),
    )));
    host.register(Box::new(AttributionModule::new(
        "attribution",
        Box::new(MemStore::new()),
    )));
    host.register(Box::new(
        Forge::init("forge", base.0.clone())
            .unwrap()
            .with_attribution("attribution")
            .with_chain_id("sources"),
    ));
    host.register(Box::new(Executor));
    for byte in [1, 2] {
        apply(
            &mut host,
            key(byte),
            "identity",
            IdentityMsg::Create {
                name: format!("person-{byte}"),
                scheme: KeyScheme::Ed25519,
            },
        )
        .await;
    }
    apply(
        &mut host,
        Origin::Module("executor".into()),
        "identity",
        IdentityMsg::CreateProgram {
            controller: 1,
            name: "program".into(),
            request: 0,
        },
    )
    .await;
    host
}
fn push(branch: &str, prev: Option<u8>, next: Option<u8>) -> ForgeMsg {
    ForgeMsg::PushRefs {
        repo: "demo".into(),
        updates: vec![RefUpdate {
            ref_name: branch.into(),
            prev_oid: prev.map(|byte| vec![byte; 20]),
            new_oid: next.map(|byte| vec![byte; 20]),
        }],
        pack_digest: next.map(|_| vec![9; 32]),
        tags: Vec::new(),
        cert: None,
    }
}
fn issue(title: &str) -> ForgeMsg {
    ForgeMsg::OpenIssue {
        repo: "demo".into(),
        title: title.into(),
        body: String::new(),
    }
}
async fn relations(host: &Host, kind: &str, object: serde_json::Value) -> ObjectRelations {
    let bytes = host
        .query(
            "attribution",
            &attribution::encode_query(&AttributionQuery::Relations {
                source: Source {
                    module: "forge".into(),
                    kind: kind.into(),
                    object: object.to_string(),
                },
            }),
        )
        .await
        .unwrap();
    let AttributionReply::Relations(Some(relations)) = attribution::decode_reply(&bytes).unwrap()
    else {
        panic!("source relations")
    };
    relations
}
async fn item(host: &Host, number: u64) -> forge::ItemDetail {
    let bytes = host
        .query(
            "forge",
            &forge::encode_query(&ForgeQuery::GetItem {
                repo: "demo".into(),
                number,
            }),
        )
        .await
        .unwrap();
    let ForgeReply::Item(Some(item)) = forge::decode_reply(&bytes).unwrap() else {
        panic!("item")
    };
    *item
}

#[test]
fn program_repo_pr_review_and_ref_lifecycle_publish_full_sets() {
    block_on(async {
        let base = Directory::new("lifecycle");
        let mut host = boot(&base).await;
        apply(
            &mut host,
            Origin::Program(3),
            "forge",
            push("main", None, Some(1)),
        )
        .await;
        let main = relations(&host, "ref", serde_json::json!(["demo", "main"])).await;
        assert_eq!(main.relations[0].recipient, 3);
        assert_eq!(main.relations[0].detail, vec![1; 20]);

        let member_push = host
            .submit_at(
                context(key(1)),
                message("forge", &push("feature", None, Some(2))),
            )
            .await;
        assert!(member_push.is_err(), "only the program owner may push refs");
        apply(
            &mut host,
            Origin::Program(3),
            "forge",
            push("feature", None, Some(2)),
        )
        .await;
        apply(
            &mut host,
            Origin::Program(3),
            "forge",
            ForgeMsg::OpenPr {
                repo: "demo".into(),
                title: "program PR".into(),
                body: String::new(),
                source_branch: "feature".into(),
                target_branch: "main".into(),
            },
        )
        .await;
        assert_eq!(item(&host, 1).await.summary.author, Party::Account(3));
        apply(
            &mut host,
            key(2),
            "forge",
            ForgeMsg::SubmitReview {
                repo: "demo".into(),
                number: 1,
                verdict: ReviewVerdict::Approve,
                body: "review".into(),
                commit_oid: "02".repeat(20),
                comments: Vec::new(),
            },
        )
        .await;
        let credited = relations(&host, "item", serde_json::json!(["demo", 1])).await;
        assert!(
            credited
                .relations
                .iter()
                .any(|relation| relation.recipient == 3 && relation.reason == Reason::Authorship)
        );
        assert!(
            credited
                .relations
                .iter()
                .any(|relation| relation.recipient == 2 && relation.reason == Reason::Credit)
        );
        assert_eq!(
            relations(&host, "review", serde_json::json!(["demo", 1, 1]))
                .await
                .relations[0]
                .recipient,
            2
        );

        let merge = ForgeMsg::MergePr {
            repo: "demo".into(),
            number: 1,
            prev_target_oid: "01".repeat(20),
            expected_source_oid: "02".repeat(20),
            merge_oid: "03".repeat(20),
            pack_digest: "09".repeat(32),
        };
        let reviewer_merge = host
            .submit_at(context(key(2)), message("forge", &merge))
            .await;
        assert!(reviewer_merge.is_err(), "a reviewer cannot merge refs");
        apply(&mut host, Origin::Program(3), "forge", merge).await;
        assert_eq!(item(&host, 1).await.summary.state, forge::ItemState::Merged);
        let merged = relations(&host, "item", serde_json::json!(["demo", 1])).await;
        assert_eq!(merged.relations, credited.relations);
        assert!(merged.revision > credited.revision);
        let reviewer_delete = host
            .submit_at(
                context(key(2)),
                message("forge", &push("feature", Some(2), None)),
            )
            .await;
        assert!(reviewer_delete.is_err(), "a reviewer cannot delete refs");
        let removed = relations(&host, "ref", serde_json::json!(["demo", "feature"])).await;
        assert_eq!(removed.relations[0].recipient, 3);
        apply(
            &mut host,
            Origin::Program(3),
            "forge",
            push("feature", Some(2), None),
        )
        .await;
        let deleted = relations(&host, "ref", serde_json::json!(["demo", "feature"])).await;
        assert!(deleted.relations.is_empty());
        apply(
            &mut host,
            Origin::Program(3),
            "forge",
            push("feature", None, Some(4)),
        )
        .await;
        let recreated = relations(&host, "ref", serde_json::json!(["demo", "feature"])).await;
        assert!(recreated.revision > removed.revision);
        assert_eq!(recreated.relations[0].recipient, 3);
    });
}

#[test]
fn rejected_central_publication_rolls_back_the_odb_source_and_number() {
    block_on(async {
        let base = Directory::new("rejected");
        let mut host = boot(&base).await;
        apply(
            &mut host,
            Origin::Module("forge".into()),
            "attribution",
            attribution::AttributionMsg::Attribute {
                object: attribution::ObjectRef {
                    kind: "ref".into(),
                    object: serde_json::json!(["demo", "main"]).to_string(),
                },
                revision: 100,
                actor: Actor::System,
                relations: Vec::new(),
                transfers: Vec::new(),
            },
        )
        .await;
        let root = host.root_hash();
        assert!(
            host.submit_at(
                context(Origin::Program(3)),
                message("forge", &push("main", None, Some(1)))
            )
            .await
            .is_err()
        );
        assert_eq!(host.root_hash(), root);
        apply(&mut host, Origin::Program(3), "forge", issue("accepted")).await;
        assert_eq!(item(&host, 1).await.summary.title, "accepted");
        assert_eq!(
            relations(&host, "item", serde_json::json!(["demo", 1]))
                .await
                .revision,
            1
        );
        drop(host);
        let reopened = Forge::init("forge", base.0.clone()).unwrap();
        let bytes = reopened
            .query(&forge::encode_query(&ForgeQuery::GetItem {
                repo: "demo".into(),
                number: 1,
            }))
            .await
            .unwrap();
        let ForgeReply::Item(Some(item)) = forge::decode_reply(&bytes).unwrap() else {
            panic!("durable item")
        };
        assert_eq!(item.summary.title, "accepted");
    });
}

fn test_ctx(origin: Origin) -> TestCtx {
    TestCtx::with_env(sdk::Env {
        height: 1,
        consensus_time: 1,
        origin,
        me: "forge".into(),
        cause: sdk::Cause::Direct,
    })
}
fn revision(ctx: &TestCtx) -> u64 {
    let attribution::AttributionMsg::AttributeBatch { updates } =
        attribution::decode_msg(&ctx.msgs().last().unwrap().payload).unwrap()
    else {
        panic!("batch")
    };
    updates[0].revision
}

#[test]
fn snapshot_reopen_and_guest_reentry_keep_source_revisions() {
    block_on(async {
        let base = Directory::new("durable");
        let copy = Directory::new("snapshot-copy");
        let commit = forge::testkit::history(
            "attribution-durable",
            &[(1, "hello.txt", "on chain", "commit")],
        )
        .pop()
        .unwrap();
        let blobs = blobstore::BlobHandle::default();
        let digest = blobs.put_chunk(commit.pack);
        let mut forge = Forge::with_blobs("forge", base.0.clone(), blobs)
            .unwrap()
            .with_attribution("attribution");
        let mut first = test_ctx(Origin::Program(3));
        forge
            .execute(&mut first, &message("forge", &issue("one")))
            .await
            .unwrap();
        assert_eq!(revision(&first), 1);
        let mut ref_ctx = test_ctx(Origin::Program(3));
        forge
            .execute(
                &mut ref_ctx,
                &message(
                    "forge",
                    &ForgeMsg::PushRefs {
                        repo: "demo".into(),
                        updates: vec![RefUpdate {
                            ref_name: "main".into(),
                            prev_oid: None,
                            new_oid: Some(commit.head.clone()),
                        }],
                        pack_digest: Some(digest.to_vec()),
                        tags: Vec::new(),
                        cert: None,
                    },
                ),
            )
            .await
            .unwrap();
        assert_eq!(revision(&ref_ctx), 2);
        forge.commit_block().await.unwrap();
        let root = forge.root();
        let snapshot = forge.snapshot().unwrap();
        let mut installed = Forge::init("forge", copy.0.clone())
            .unwrap()
            .with_attribution("attribution");
        installed.install(&snapshot, root).unwrap();
        assert_eq!(installed.root(), root);
        drop(forge);
        let mut reopened = Forge::init("forge", base.0.clone())
            .unwrap()
            .with_attribution("attribution");
        for module in [&mut reopened, &mut installed] {
            let mut ctx = test_ctx(Origin::Program(3));
            module
                .execute(&mut ctx, &message("forge", &issue("two")))
                .await
                .unwrap();
            assert_eq!(revision(&ctx), 3);
            module.commit_block().await.unwrap();
        }
        assert_eq!(reopened.root(), installed.root());
        for path in [&base.0, &copy.0] {
            let git = git2::Repository::open(path.join("demo")).unwrap();
            let head = git.refname_to_id("refs/heads/main").unwrap();
            assert_eq!(head.as_bytes(), commit.head.as_slice());
            let tree = git.find_commit(head).unwrap().tree().unwrap();
            let entry = tree.get_name("hello.txt").unwrap();
            assert_eq!(git.find_blob(entry.id()).unwrap().content(), b"on chain");
        }

        let mut state = forge::state::ForgeState::default();
        let mut ctx = test_ctx(Origin::Program(3));
        state
            .apply(
                &mut ctx,
                &forge::encode_msg(&issue("one")),
                None,
                Some("attribution"),
                "sources",
            )
            .await
            .unwrap();
        let image = forge::state::decode_image(&state.published_image()).unwrap();
        let mut reentered = forge::state::ForgeState::from_lane(image, state.block_scratch());
        let mut ctx = test_ctx(Origin::Program(3));
        reentered
            .apply(
                &mut ctx,
                &forge::encode_msg(&issue("two")),
                None,
                Some("attribution"),
                "sources",
            )
            .await
            .unwrap();
        assert_eq!(revision(&ctx), 2);
        assert_eq!(reentered.tracker_view().repos["demo"].items.len(), 2);
    });
}

#[test]
fn failed_multi_ref_cas_restores_incoming_staging() {
    block_on(async {
        let mut state = forge::state::ForgeState::default();
        let mut ctx = test_ctx(Origin::Program(3));
        state
            .apply(
                &mut ctx,
                &forge::encode_msg(&issue("keep")),
                None,
                Some("attribution"),
                "sources",
            )
            .await
            .unwrap();
        let before = state.published_image();
        let mut failed = push("first", None, Some(1));
        let ForgeMsg::PushRefs { updates, .. } = &mut failed else {
            unreachable!()
        };
        updates.push(RefUpdate {
            ref_name: "second".into(),
            prev_oid: Some(vec![2; 20]),
            new_oid: Some(vec![3; 20]),
        });
        assert!(
            state
                .apply(
                    &mut test_ctx(Origin::Program(3)),
                    &forge::encode_msg(&failed),
                    None,
                    Some("attribution"),
                    "sources"
                )
                .await
                .is_err()
        );
        assert_eq!(state.published_image(), before);
        assert!(state.block_scratch().is_empty());
        let mut ctx = test_ctx(Origin::Program(3));
        state
            .apply(
                &mut ctx,
                &forge::encode_msg(&push("first", None, Some(1))),
                None,
                Some("attribution"),
                "sources",
            )
            .await
            .unwrap();
        assert_eq!(revision(&ctx), 2);
    });
}

#[test]
fn item_authorship_records_the_signers_canonical_actor() {
    block_on(async {
        let base = Directory::new("key-rights");
        let mut forge = Forge::init("forge", base.0.clone())
            .unwrap()
            .with_attribution("attribution");
        let context = |byte, account: Option<u64>| {
            test_ctx(key(byte)).on_query("identity", move |req| {
                let identity::IdentityQuery::OfKey { key } = identity::decode_query(req).unwrap()
                else {
                    panic!("key lookup");
                };
                let resolved = (key == vec![byte; 32]).then_some(account).flatten();
                Ok(identity::encode_reply(&identity::IdentityReply::Account(
                    resolved.map(|number| identity::AccountView {
                        number,
                        name: "person".into(),
                        control: identity::Control::Keys,
                        keys: Vec::new(),
                        avatar: None,
                        bio: None,
                        updated_at: 0,
                    }),
                )))
            })
        };
        forge
            .execute(
                &mut context(9, Some(1)),
                &message("forge", &push("main", None, Some(1))),
            )
            .await
            .unwrap();
        forge
            .execute(
                &mut context(11, Some(2)),
                &message("forge", &issue("key owned")),
            )
            .await
            .unwrap();
        forge.commit_block().await.unwrap();
        let edit = ForgeMsg::EditItem {
            repo: "demo".into(),
            number: 1,
            title: Some("owner edit".into()),
            body: None,
        };
        forge
            .execute(&mut context(10, Some(1)), &message("forge", &edit))
            .await
            .unwrap();
        forge.commit_block().await.unwrap();
        forge
            .execute(
                &mut context(11, Some(2)),
                &message(
                    "forge",
                    &ForgeMsg::EditItem {
                        repo: "demo".into(),
                        number: 1,
                        title: Some("author edit".into()),
                        body: None,
                    },
                ),
            )
            .await
            .unwrap();
        forge.commit_block().await.unwrap();
        for (index, account) in [None, Some(3)].into_iter().enumerate() {
            let edit = ForgeMsg::EditItem {
                repo: "demo".into(),
                number: 1,
                title: Some(format!("rejected edit {index}")),
                body: None,
            };
            assert!(
                forge
                    .execute(&mut context(10, account), &message("forge", &edit))
                    .await
                    .is_err()
            );
            forge.abort_block().await.unwrap();
        }
        for (index, account) in [None, Some(2)].into_iter().enumerate() {
            forge
                .execute(
                    &mut context(9, account),
                    &message("forge", &issue("new actor")),
                )
                .await
                .unwrap();
            forge.commit_block().await.unwrap();
            let bytes = forge
                .query(&forge::encode_query(&ForgeQuery::GetItem {
                    repo: "demo".into(),
                    number: index as u64 + 2,
                }))
                .await
                .unwrap();
            let ForgeReply::Item(Some(item)) = forge::decode_reply(&bytes).unwrap() else {
                panic!("new item")
            };
            assert_eq!(
                item.summary.author,
                account.map_or_else(|| Party::Key(vec![9; 32]), Party::Account)
            );
        }
    });
}

#[test]
fn ref_attribution_settles_with_the_push_and_a_refused_push_publishes_nothing() {
    block_on(async {
        let context = |byte, accounts: Vec<(u8, u64)>| {
            test_ctx(key(byte)).on_query("identity", move |req| {
                let identity::IdentityQuery::OfKey { key } = identity::decode_query(req).unwrap()
                else {
                    panic!("key lookup");
                };
                let account = accounts
                    .iter()
                    .find_map(|(byte, number)| (key == vec![*byte; 32]).then_some(*number));
                Ok(identity::encode_reply(&identity::IdentityReply::Account(
                    account.map(|number| identity::AccountView {
                        number,
                        name: "person".into(),
                        control: identity::Control::Keys,
                        keys: Vec::new(),
                        avatar: None,
                        bio: None,
                        updated_at: 0,
                    }),
                )))
            })
        };
        let mut state = forge::state::ForgeState::default();
        state
            .apply(
                &mut context(9, vec![(9, 1), (10, 1), (11, 2)]),
                &forge::encode_msg(&push("main", None, Some(1))),
                None,
                Some("attribution"),
                "sources",
            )
            .await
            .unwrap();
        state = forge::state::ForgeState::from_lane(
            forge::state::decode_image(&state.published_image()).unwrap(),
            Default::default(),
        );
        let before = state.published_image();
        let accounts = vec![(9, 1), (10, 1), (11, 2)];
        // the CAS fails after the party lookup: nothing is published and no
        // report leaves the module.
        let mut ctx = context(10, accounts.clone());
        assert!(
            state
                .apply(
                    &mut ctx,
                    &forge::encode_msg(&push("main", Some(8), Some(2))),
                    None,
                    Some("attribution"),
                    "sources"
                )
                .await
                .is_err()
        );
        assert_eq!(state.published_image(), before);
        assert!(ctx.msgs().is_empty());
        // a member other than the one who birthed the repo is refused, and
        // the committed image remains unchanged.
        let mut ctx = context(11, accounts.clone());
        assert!(
            state
                .apply(
                    &mut ctx,
                    &forge::encode_msg(&push("main", Some(1), Some(2))),
                    None,
                    Some("attribution"),
                    "sources",
                )
                .await
                .is_err()
        );
        assert!(ctx.msgs().is_empty());
        state.abort();
        // the owner key resolves to ACCOUNT 1, and its report names that
        // canonical actor.
        let mut ctx = context(10, accounts.clone());
        state
            .apply(
                &mut ctx,
                &forge::encode_msg(&push("main", Some(1), Some(2))),
                None,
                Some("attribution"),
                "sources",
            )
            .await
            .unwrap();
        let attribution::AttributionMsg::AttributeBatch { updates } =
            attribution::decode_msg(&ctx.msgs()[0].payload).unwrap()
        else {
            panic!("source report");
        };
        let main = updates
            .iter()
            .find(|update| update.object.kind == "ref")
            .unwrap();
        assert_eq!(main.actor, Actor::Account(1));
        assert_eq!(
            main.relations,
            vec![attribution::Relation {
                recipient: 1,
                reason: Reason::Defined("ref_writer".into()),
                detail: vec![2; 20],
            }]
        );
        state.abort();
        assert_eq!(
            state.published_image(),
            before,
            "an aborted push is invisible"
        );
    });
}

#[test]
fn signed_push_attributes_the_real_account_signer_instead_of_its_relay() {
    block_on(async {
        let base = Directory::new("signed");
        let mut host = boot(&base).await;
        let signer = keyscheme::testkit::ssh_key(8);
        let public_key = keyscheme::testkit::ssh_pubkey(&signer);
        apply(
            &mut host,
            Origin::External(public_key),
            "identity",
            IdentityMsg::Create {
                name: "ssh-owner".into(),
                scheme: KeyScheme::Ed25519,
            },
        )
        .await;
        let mut signed = push("main", None, Some(1));
        let ForgeMsg::PushRefs { updates, cert, .. } = &mut signed else {
            unreachable!()
        };
        let bytes =
            forge::pushcert::certificate(&forge::pushcert::nonce("sources", "demo"), updates, &[]);
        *cert = Some(forge::PushCert {
            sshsig: keyscheme::testkit::sshsig(&signer, keyscheme::sshsig::GIT_SSH_NS, &bytes),
            cert: bytes,
        });
        apply(&mut host, key(1), "forge", signed).await;
        assert_eq!(
            relations(&host, "ref", serde_json::json!(["demo", "main"]))
                .await
                .relations[0]
                .recipient,
            4
        );
        let bytes = host
            .query(
                "attribution",
                &attribution::encode_query(&AttributionQuery::ChangesOf {
                    source: Source {
                        module: "forge".into(),
                        kind: "ref".into(),
                        object: serde_json::json!(["demo", "main"]).to_string(),
                    },
                    after: 0,
                    limit: 10,
                }),
            )
            .await
            .unwrap();
        let AttributionReply::Changes(changes) = attribution::decode_reply(&bytes).unwrap() else {
            panic!("changes")
        };
        assert_eq!(changes[0].change.actor, Actor::Account(4));
    });
}
