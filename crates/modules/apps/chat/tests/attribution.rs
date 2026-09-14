//! Real source → attribution transactions, with stable identity and rollback.
use attribution::{
    Actor, AttributionModule, AttributionMsg, AttributionQuery, AttributionReply, ObjectRef,
    ObjectRelations, Reason, Source,
};
use chat::{Block, Chat, ChatMsg, ChatQuery, ChatReply, Mark, Party, PostPolicy, Span};
use commonware_cryptography::{Signer as _, ed25519};
use futures::executor::block_on;
use host::{BlockContext, Host};
use identity::{Identity, IdentityMsg, KeyScheme};
use pages::{InlineMark, NewBlock, PageMsg, Pages, SpanMark};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};
use sdk_testkit::{MemStore, TestCtx};

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
            .map_err(Error::Module)?;
        Ok(())
    }
}
fn context(origin: Origin) -> BlockContext {
    BlockContext {
        height: 1,
        consensus_time: 1,
        origin,
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
async fn apply<T: serde::Serialize>(host: &mut Host, origin: Origin, target: &str, input: T) {
    host.submit_at(context(origin), message(target, &input))
        .await
        .unwrap();
}
async fn boot() -> Host {
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
        Chat::new("chat", Box::new(MemStore::new()))
            .with_identity("identity")
            .with_attribution("attribution"),
    ));
    host.register(Box::new(
        Pages::new("pages", Box::new(MemStore::new()))
            .with_identity("identity")
            .with_attribution("attribution"),
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
async fn relations(host: &Host, module: &str, kind: &str, object: &str) -> ObjectRelations {
    let bytes = host
        .query(
            "attribution",
            &attribution::encode_query(&AttributionQuery::Relations {
                source: Source {
                    module: module.into(),
                    kind: kind.into(),
                    object: object.into(),
                },
            }),
        )
        .await
        .unwrap();
    let AttributionReply::Relations(Some(relations)) = attribution::decode_reply(&bytes).unwrap()
    else {
        panic!("recorded source")
    };
    relations
}
fn body(account: u64) -> Vec<Block> {
    vec![Block::Paragraph(vec![Span {
        text: "hello".into(),
        marks: vec![
            Mark::Mention(Party::Account(account)),
            Mark::Mention(Party::Account(account)),
        ],
    }])]
}
fn post(id: &str, blocks: Vec<Block>) -> ChatMsg {
    ChatMsg::PostMessage {
        channel_id: "room".into(),
        message_id: id.into(),
        blocks,
        thread: None,
    }
}

#[test]
fn chat_full_relation_sets_and_program_authority() {
    block_on(async {
        let mut host = boot().await;
        apply(
            &mut host,
            key(1),
            "chat",
            ChatMsg::CreateChannel {
                channel_id: "room".into(),
                name: "Room".into(),
                post_policy: PostPolicy::Open,
            },
        )
        .await;
        apply(&mut host, key(1), "chat", post("m", body(3))).await;
        let first = relations(&host, "chat", "message", "m").await;
        assert_eq!(first.revision, 1);
        assert_eq!(
            first
                .relations
                .iter()
                .map(|r| (r.recipient, &r.reason))
                .collect::<Vec<_>>(),
            vec![(1, &Reason::Authorship), (3, &Reason::Mention)]
        );
        apply(
            &mut host,
            key(1),
            "chat",
            ChatMsg::EditMessage {
                channel_id: "room".into(),
                seq: 1,
                blocks: body(2),
                base_rev: None,
            },
        )
        .await;
        let edited = relations(&host, "chat", "message", "m").await;
        assert_eq!(edited.revision, 2);
        assert_eq!(edited.changes, 4, "retained authorship is not a new event");
        assert!(
            edited
                .relations
                .iter()
                .any(|r| r.recipient == 2 && r.reason == Reason::Mention)
        );
        assert!(!edited.relations.iter().any(|r| r.recipient == 3));
        apply(
            &mut host,
            key(1),
            "chat",
            ChatMsg::DeleteMessage {
                channel_id: "room".into(),
                seq: 1,
            },
        )
        .await;
        assert!(
            relations(&host, "chat", "message", "m")
                .await
                .relations
                .is_empty()
        );
        apply(
            &mut host,
            Origin::Program(3),
            "chat",
            post("program", body(1)),
        )
        .await;
        let bytes = host
            .query(
                "chat",
                &chat::encode_query(&ChatQuery::Message {
                    message_id: "program".into(),
                }),
            )
            .await
            .unwrap();
        let ChatReply::Message(Some(view)) = chat::decode_reply(&bytes).unwrap() else {
            panic!("message")
        };
        assert_eq!(view.head.author, Party::Account(3));
        assert_eq!(view.head.origin, Origin::Program(3));
        let edit = ChatMsg::EditMessage {
            channel_id: "room".into(),
            seq: 2,
            blocks: vec![Block::paragraph("edited")],
            base_rev: None,
        };
        apply(&mut host, Origin::Program(3), "chat", edit).await;
        let bytes = host
            .query(
                "attribution",
                &attribution::encode_query(&AttributionQuery::ChangesOf {
                    source: Source {
                        module: "chat".into(),
                        kind: "message".into(),
                        object: "program".into(),
                    },
                    after: 0,
                    limit: 256,
                }),
            )
            .await
            .unwrap();
        let AttributionReply::Changes(changes) = attribution::decode_reply(&bytes).unwrap() else {
            panic!("history")
        };
        assert!(
            changes
                .iter()
                .all(|entry| entry.change.actor == Actor::Account(3))
        );
        apply(&mut host, key(99), "chat", post("node", body(1))).await;
        let bytes = host
            .query(
                "chat",
                &chat::encode_query(&ChatQuery::Message {
                    message_id: "node".into(),
                }),
            )
            .await
            .unwrap();
        let ChatReply::Message(Some(view)) = chat::decode_reply(&bytes).unwrap() else {
            panic!("node message")
        };
        assert_eq!(view.head.author, Party::Key(vec![99; 32]));
        assert_eq!(view.head.origin, key(99));
    });
}

#[test]
fn a_rejected_attribution_aborts_its_source_write() {
    block_on(async {
        let mut host = boot().await;
        apply(
            &mut host,
            key(1),
            "chat",
            ChatMsg::CreateChannel {
                channel_id: "room".into(),
                name: "Room".into(),
                post_policy: PostPolicy::Open,
            },
        )
        .await;
        apply(
            &mut host,
            Origin::Module("chat".into()),
            "attribution",
            AttributionMsg::Attribute {
                object: ObjectRef {
                    kind: "message".into(),
                    object: "reserved".into(),
                },
                revision: 100,
                actor: Actor::System,
                relations: Vec::new(),
                transfers: Vec::new(),
            },
        )
        .await;
        let before = host.root_hash();
        assert!(
            host.submit_at(context(key(1)), message("chat", &post("reserved", body(3))))
                .await
                .is_err()
        );
        assert_eq!(host.root_hash(), before);
        let bytes = host
            .query(
                "chat",
                &chat::encode_query(&ChatQuery::Message {
                    message_id: "reserved".into(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            chat::decode_reply(&bytes).unwrap(),
            ChatReply::Message(None)
        );
    });
}

#[test]
fn pages_text_and_comment_edits_remove_mentions_and_subtree_purge_retires_relations() {
    block_on(async {
        let mut host = boot().await;
        apply(
            &mut host,
            Origin::Program(3),
            "pages",
            PageMsg::CreatePage {
                page_id: "p".into(),
                title: "Program's page".into(),
                blocks: Vec::new(),
            },
        )
        .await;
        apply(
            &mut host,
            Origin::Program(3),
            "pages",
            PageMsg::InsertBlock {
                parent: "p".into(),
                after: None,
                block: NewBlock {
                    id: "b".into(),
                    kind: pages::BlockKind::Paragraph,
                    text: "tag".into(),
                    marks: vec![SpanMark {
                        start: 0,
                        end: 3,
                        kind: InlineMark::Mention(1),
                    }],
                },
            },
        )
        .await;
        assert!(
            relations(&host, "pages", "block", "b")
                .await
                .relations
                .iter()
                .any(|r| r.recipient == 1 && r.reason == Reason::Mention)
        );
        let update = PageMsg::UpdateText {
            block_id: "b".into(),
            text: "next".into(),
            marks: Some(Vec::new()),
        };
        apply(&mut host, Origin::Program(3), "pages", update).await;
        assert_eq!(
            relations(&host, "pages", "block", "b")
                .await
                .relations
                .len(),
            1
        );
        apply(
            &mut host,
            key(2),
            "pages",
            PageMsg::AddComment {
                thread_id: "t".into(),
                comment_id: "c".into(),
                target: "b".into(),
                text: "ping".into(),
                anchor: None,
                mentions: vec![3],
            },
        )
        .await;
        apply(
            &mut host,
            key(2),
            "pages",
            PageMsg::EditComment {
                comment_id: "c".into(),
                text: "changed".into(),
                mentions: vec![1],
            },
        )
        .await;
        let comment = relations(&host, "pages", "comment", "c").await;
        assert!(comment.revision > 1);
        assert_eq!(comment.changes, 4);
        assert!(!comment.relations.iter().any(|r| r.recipient == 3));
        apply(
            &mut host,
            Origin::Program(3),
            "pages",
            PageMsg::RemoveBlock {
                block_id: "p".into(),
            },
        )
        .await;
        for (kind, id) in [("block", "p"), ("block", "b"), ("comment", "c")] {
            assert!(
                relations(&host, "pages", kind, id)
                    .await
                    .relations
                    .is_empty()
            );
        }
        let deleted = relations(&host, "pages", "block", "p").await.revision;
        apply(
            &mut host,
            key(1),
            "pages",
            PageMsg::CreatePage {
                page_id: "p".into(),
                title: "Recreated".into(),
                blocks: Vec::new(),
            },
        )
        .await;
        let recreated = relations(&host, "pages", "block", "p").await;
        assert!(recreated.revision > deleted);
        assert!(
            recreated
                .relations
                .iter()
                .all(|relation| relation.recipient == 1)
        );
    });
}

#[test]
fn a_local_page_rejection_preserves_previous_staging_without_abort() {
    block_on(async {
        let mut pages =
            Pages::new("pages", Box::new(MemStore::new())).with_attribution("attribution");
        let mut ctx = TestCtx::at_height(1);
        pages
            .execute(
                &mut ctx,
                &message(
                    "pages",
                    &PageMsg::CreatePage {
                        page_id: "kept".into(),
                        title: "Kept".into(),
                        blocks: Vec::new(),
                    },
                ),
            )
            .await
            .unwrap();
        assert!(
            pages
                .execute(
                    &mut ctx,
                    &message(
                        "pages",
                        &PageMsg::CreatePage {
                            page_id: "bad".into(),
                            title: "x".repeat(pages::MAX_PAGE_TITLE_LEN + 1),
                            blocks: Vec::new(),
                        }
                    )
                )
                .await
                .is_err()
        );
        pages.commit_block().await.unwrap();
        for (id, exists) in [("kept", true), ("bad", false)] {
            let bytes = pages
                .query(&pages::encode_query(&pages::PageQuery::GetBlock {
                    block_id: id.into(),
                }))
                .await
                .unwrap();
            let pages::PageReply::Block(block) = pages::decode_reply(&bytes).unwrap() else {
                panic!("block")
            };
            assert_eq!(block.is_some(), exists);
        }
    });
}

#[test]
fn joining_identity_preserves_the_original_keys_source_rights() {
    block_on(async {
        let mut host = boot().await;
        let node = ed25519::PrivateKey::from_seed(99);
        let node_proof = node
            .sign(
                chat::HUDDLE_JOIN_NS,
                &chat::huddle_join_preimage("room", &[99; 32]),
            )
            .as_ref()
            .to_vec();
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::CreateChannel {
                channel_id: "room".into(),
                name: "Key room".into(),
                post_policy: PostPolicy::MembersOnly,
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::SetMembership {
                channel_id: "room".into(),
                party: Party::Key(vec![99; 32]),
                member: true,
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "chat",
            post("old", vec![Block::paragraph("before admission")]),
        )
        .await;
        apply(
            &mut host,
            key(99),
            "pages",
            PageMsg::CreatePage {
                page_id: "key-page".into(),
                title: "Before".into(),
                blocks: Vec::new(),
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "pages",
            PageMsg::AddComment {
                thread_id: "key-thread".into(),
                comment_id: "key-comment".into(),
                target: "key-page".into(),
                text: "Before".into(),
                anchor: None,
                mentions: Vec::new(),
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::AddReaction {
                channel_id: "room".into(),
                seq: 1,
                emoji: "yes".into(),
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::JoinHuddle {
                channel_id: "room".into(),
                node: node.public_key().as_ref().to_vec(),
                node_proof: node_proof.clone(),
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "identity",
            IdentityMsg::Create {
                name: "new-account".into(),
                scheme: KeyScheme::Ed25519,
            },
        )
        .await;
        let before = host.root_hash();
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::AddReaction {
                channel_id: "room".into(),
                seq: 1,
                emoji: "yes".into(),
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::JoinHuddle {
                channel_id: "room".into(),
                node: node.public_key().as_ref().to_vec(),
                node_proof: node_proof.clone(),
            },
        )
        .await;
        assert_eq!(
            host.root_hash(),
            before,
            "admission does not duplicate historic participation"
        );
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::RemoveReaction {
                channel_id: "room".into(),
                seq: 1,
                emoji: "yes".into(),
            },
        )
        .await;
        let swept = host
            .submit_at(
                context(key(99)),
                message(
                    "chat",
                    &ChatMsg::SweepHuddle {
                        channel_id: "room".into(),
                        party: Party::Account(4),
                    },
                ),
            )
            .await
            .unwrap();
        let stamp = chat::decode_assigned(&swept.dispatches[0].assigned).unwrap();
        assert_eq!(stamp.actor(), &Party::Account(4));
        assert_eq!(
            stamp.participant().unwrap(),
            &Party::Key(vec![99; 32]),
            "self-sweep publishes the actual historic entry removed"
        );
        let bytes = host
            .query(
                "chat",
                &chat::encode_query(&ChatQuery::Channel {
                    channel_id: "room".into(),
                }),
            )
            .await
            .unwrap();
        let ChatReply::Channel(Some(room)) = chat::decode_reply(&bytes).unwrap() else {
            panic!("room")
        };
        assert!(
            room.huddle.is_empty(),
            "the authenticated key can leave its historic entry"
        );
        let added = host
            .submit_at(
                context(key(99)),
                message(
                    "chat",
                    &ChatMsg::AddReaction {
                        channel_id: "room".into(),
                        seq: 1,
                        emoji: "yes".into(),
                    },
                ),
            )
            .await
            .unwrap();
        let assigned = chat::decode_assigned(&added.dispatches[0].assigned).unwrap();
        assert_eq!(
            assigned.participant().unwrap(),
            &Party::Account(4),
            "removing the historic key reaction allows the next add to use its account"
        );
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::RemoveReaction {
                channel_id: "room".into(),
                seq: 1,
                emoji: "yes".into(),
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::RenameChannel {
                channel_id: "room".into(),
                name: "After".into(),
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::EditMessage {
                channel_id: "room".into(),
                seq: 1,
                blocks: vec![Block::paragraph("after admission")],
                base_rev: None,
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "chat",
            post("new", vec![Block::paragraph("membership survives")]),
        )
        .await;
        apply(
            &mut host,
            key(99),
            "pages",
            PageMsg::UpdateText {
                block_id: "key-page".into(),
                text: "After".into(),
                marks: None,
            },
        )
        .await;
        apply(
            &mut host,
            key(99),
            "pages",
            PageMsg::EditComment {
                comment_id: "key-comment".into(),
                text: "After".into(),
                mentions: Vec::new(),
            },
        )
        .await;
        let bytes = host
            .query(
                "chat",
                &chat::encode_query(&ChatQuery::Message {
                    message_id: "new".into(),
                }),
            )
            .await
            .unwrap();
        let ChatReply::Message(Some(view)) = chat::decode_reply(&bytes).unwrap() else {
            panic!("new message")
        };
        assert_eq!(
            view.head.author,
            Party::Account(4),
            "new writes carry the canonical account"
        );
        assert_eq!(
            view.head.origin,
            key(99),
            "canonical authorship preserves the actual signer"
        );
        apply(
            &mut host,
            key(99),
            "chat",
            ChatMsg::SetMembership {
                channel_id: "room".into(),
                party: Party::Key(vec![99; 32]),
                member: false,
            },
        )
        .await;
        assert!(
            host.submit_at(
                context(key(99)),
                message(
                    "chat",
                    &post("revoked-membership", vec![Block::paragraph("refused")])
                )
            )
            .await
            .is_err(),
            "the original key's historic membership remains revocable after admission"
        );
    });
}

#[test]
fn edits_keep_original_provenance_and_stamp_the_current_content_signer() {
    block_on(async {
        let mut chat = Chat::new("chat", Box::new(MemStore::new())).with_identity("identity");
        let context = |byte| {
            TestCtx::with_env(sdk::Env {
                height: 1,
                consensus_time: 1,
                origin: key(byte),
                me: "chat".into(),
                cause: sdk::Cause::Direct,
            })
            .on_query("identity", |_| {
                Ok(identity::encode_reply(&identity::IdentityReply::Account(
                    Some(identity::AccountView {
                        number: 1,
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
        chat.execute(
            &mut context(9),
            &message(
                "chat",
                &ChatMsg::CreateChannel {
                    channel_id: "room".into(),
                    name: "Room".into(),
                    post_policy: PostPolicy::Open,
                },
            ),
        )
        .await
        .unwrap();
        chat.execute(
            &mut context(9),
            &message("chat", &post("command", vec![Block::paragraph("original")])),
        )
        .await
        .unwrap();
        chat.commit_block().await.unwrap();
        let query = chat::encode_query(&ChatQuery::Message {
            message_id: "command".into(),
        });
        let ChatReply::Message(Some(original)) =
            chat::decode_reply(&chat.query(&query).await.unwrap()).unwrap()
        else {
            panic!("message")
        };
        assert_eq!(original.head.origin, key(9));
        assert_eq!(original.head.content_origin, key(9));
        chat.execute(
            &mut context(10),
            &message(
                "chat",
                &ChatMsg::EditMessage {
                    channel_id: "room".into(),
                    seq: 1,
                    blocks: vec![Block::paragraph("sibling edit")],
                    base_rev: Some(0),
                },
            ),
        )
        .await
        .unwrap();
        chat.commit_block().await.unwrap();
        let ChatReply::Message(Some(edited)) =
            chat::decode_reply(&chat.query(&query).await.unwrap()).unwrap()
        else {
            panic!("message")
        };
        assert_eq!(edited.head.author, Party::Account(1));
        assert_eq!(edited.head.origin, key(9));
        assert_eq!(edited.head.content_origin, key(10));
        chat.execute(
            &mut context(9),
            &message(
                "chat",
                &ChatMsg::DeleteMessage {
                    channel_id: "room".into(),
                    seq: 1,
                },
            ),
        )
        .await
        .unwrap();
        chat.commit_block().await.unwrap();
        let ChatReply::Message(Some(deleted)) =
            chat::decode_reply(&chat.query(&query).await.unwrap()).unwrap()
        else {
            panic!("message")
        };
        assert!(deleted.head.deleted);
        assert_eq!(deleted.head.origin, key(9));
        assert_eq!(deleted.head.content_origin, key(10));
    });
}

#[test]
fn key_mentions_are_frozen_as_accounts_in_canonical_heads_and_stamps() {
    block_on(async {
        let mut chat = Chat::new("chat", Box::new(MemStore::new()))
            .with_identity("identity")
            .with_attribution("attribution");
        let context = |key_owner| {
            TestCtx::at_height(1).on_query("identity", move |query| {
                let query = identity::decode_query(query).unwrap();
                if let identity::IdentityQuery::Resolve { references } = query {
                    let numbers = references
                        .into_iter()
                        .map(|reference| {
                            Some(match reference {
                                identity::AccountRef::Key(_) => key_owner,
                                identity::AccountRef::Account(number) => number,
                            })
                        })
                        .collect();
                    return Ok(identity::encode_reply(&identity::IdentityReply::Resolved(
                        numbers,
                    )));
                }
                let number = match query {
                    identity::IdentityQuery::OfKey { .. } => key_owner,
                    identity::IdentityQuery::Get { number } => number,
                    _ => panic!("identity point read"),
                };
                Ok(identity::encode_reply(&identity::IdentityReply::Account(
                    Some(identity::AccountView {
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
        chat.execute(
            &mut context(2),
            &message(
                "chat",
                &ChatMsg::CreateChannel {
                    channel_id: "room".into(),
                    name: "Room".into(),
                    post_policy: PostPolicy::Open,
                },
            ),
        )
        .await
        .unwrap();
        let input = post(
            "key",
            vec![Block::Paragraph(vec![Span {
                text: "tag".into(),
                marks: vec![Mark::Mention(Party::Key(vec![2; 32]))],
            }])],
        );
        let mut ctx = context(2);
        chat.execute(&mut ctx, &message("chat", &input))
            .await
            .unwrap();
        let canonical = vec![Block::Paragraph(vec![Span {
            text: "tag".into(),
            marks: vec![Mark::Mention(Party::Account(2))],
        }])];
        let chat::ChatAssigned::Posted { key_mentions, .. } =
            chat::decode_assigned(ctx.assigned().unwrap()).unwrap()
        else {
            panic!("post stamp")
        };
        assert_eq!(key_mentions, vec![2]);
        let bytes = chat
            .query(&chat::encode_query(&ChatQuery::Message {
                message_id: "key".into(),
            }))
            .await
            .unwrap();
        let ChatReply::Message(Some(view)) = chat::decode_reply(&bytes).unwrap() else {
            panic!("head")
        };
        assert_eq!(view.head.blocks, canonical);
        let writes = chat::index::fold_op(
            &index_guest::OpRow {
                height: 1,
                seq: 0,
                time: 1,
                origin: index_guest::OriginTag::system(),
                payload: chat::encode_msg(&input),
                assigned: ctx.assigned().unwrap().to_vec(),
            },
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        let mut map = std::collections::BTreeMap::new();
        index_guest::apply_to_map(&mut map, writes);
        let bytes = chat::index::serve_view(
            &map,
            &serde_json::to_vec(&chat::index::ChatViewQuery::Message {
                message_id: "key".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let chat::index::ChatViewReply::Message(Some(projected)) =
            serde_json::from_slice(&bytes).unwrap()
        else {
            panic!("projected message")
        };
        assert_eq!(projected.blocks, canonical);
        let mut ctx = context(3);
        chat.execute(
            &mut ctx,
            &message(
                "chat",
                &ChatMsg::EditMessage {
                    channel_id: "room".into(),
                    seq: 1,
                    blocks: canonical,
                    base_rev: None,
                },
            ),
        )
        .await
        .unwrap();
        let AttributionMsg::Attribute { relations, .. } =
            attribution::decode_msg(&ctx.msgs()[0].payload).unwrap()
        else {
            panic!("source report")
        };
        assert_eq!(
            relations[0].recipient, 2,
            "moving the key to account 3 cannot move an existing tag"
        );
    });
}

#[test]
fn attribution_batches_retire_more_than_the_host_dispatch_limit_of_sources() {
    block_on(async {
        let mut host = boot().await;
        apply(
            &mut host,
            Origin::Program(3),
            "pages",
            PageMsg::CreatePage {
                page_id: "wide".into(),
                title: "Wide page".into(),
                blocks: Vec::new(),
            },
        )
        .await;
        // Existing traversal capacity admits this subtree. Its source histories
        // must remain individually addressable without one dispatch per block.
        const CHILDREN: usize = 1_100;
        for index in 0..CHILDREN {
            apply(
                &mut host,
                Origin::Program(3),
                "pages",
                PageMsg::InsertBlock {
                    parent: "wide".into(),
                    after: None,
                    block: NewBlock {
                        id: format!("leaf-{index}"),
                        kind: pages::BlockKind::Paragraph,
                        text: "mention".into(),
                        marks: vec![SpanMark {
                            start: 0,
                            end: 7,
                            kind: InlineMark::Mention(1),
                        }],
                    },
                },
            )
            .await;
        }
        let before = relations(&host, "pages", "block", "leaf-0").await;
        assert_eq!(before.relations.len(), 2);
        apply(
            &mut host,
            Origin::Program(3),
            "pages",
            PageMsg::RemoveBlock {
                block_id: "wide".into(),
            },
        )
        .await;
        let retired_page = relations(&host, "pages", "block", "wide").await;
        assert!(retired_page.relations.is_empty());
        for index in 0..CHILDREN {
            let retired = relations(&host, "pages", "block", &format!("leaf-{index}")).await;
            assert!(retired.relations.is_empty());
            assert_eq!(retired.revision, retired_page.revision);
            assert_eq!(
                retired.changes, 4,
                "authorship and mention each add and remove"
            );
        }
        assert!(retired_page.revision > before.revision);
        let bytes = host
            .query(
                "pages",
                &pages::encode_query(&pages::PageQuery::GetBlock {
                    block_id: "wide".into(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            pages::decode_reply(&bytes).unwrap(),
            pages::PageReply::Block(None)
        );
    });
}

#[test]
fn program_huddle_proof_binds_the_node_to_its_authenticated_account() {
    block_on(async {
        let mut host = boot().await;
        apply(
            &mut host,
            key(1),
            "chat",
            ChatMsg::CreateChannel {
                channel_id: "voice".into(),
                name: "Voice".into(),
                post_policy: PostPolicy::Open,
            },
        )
        .await;
        let node = ed25519::PrivateKey::from_seed(77);
        for (namespace, preimage) in [
            (
                chat::HUDDLE_JOIN_NS,
                chat::huddle_join_preimage("voice", &[1; 32]),
            ),
            (
                chat::PROGRAM_HUDDLE_JOIN_NS,
                chat::program_huddle_join_preimage("voice", 2),
            ),
            (
                chat::PROGRAM_HUDDLE_JOIN_NS,
                chat::program_huddle_join_preimage("other", 3),
            ),
        ] {
            let before = host.root_hash();
            let rejected = host
                .submit_at(
                    context(Origin::Program(3)),
                    message(
                        "chat",
                        &ChatMsg::JoinHuddle {
                            channel_id: "voice".into(),
                            node: node.public_key().as_ref().to_vec(),
                            node_proof: node.sign(namespace, &preimage).as_ref().to_vec(),
                        },
                    ),
                )
                .await;
            assert!(rejected.is_err());
            assert_eq!(host.root_hash(), before);
        }
        apply(
            &mut host,
            Origin::Program(3),
            "chat",
            ChatMsg::JoinHuddle {
                channel_id: "voice".into(),
                node: node.public_key().as_ref().to_vec(),
                node_proof: node
                    .sign(
                        chat::PROGRAM_HUDDLE_JOIN_NS,
                        &chat::program_huddle_join_preimage("voice", 3),
                    )
                    .as_ref()
                    .to_vec(),
            },
        )
        .await;
        let bytes = host
            .query(
                "chat",
                &chat::encode_query(&ChatQuery::Channel {
                    channel_id: "voice".into(),
                }),
            )
            .await
            .unwrap();
        let ChatReply::Channel(Some(channel)) = chat::decode_reply(&bytes).unwrap() else {
            panic!("channel")
        };
        assert_eq!(channel.huddle[0].party, Party::Account(3));
        assert_eq!(channel.huddle[0].node, node.public_key().as_ref());
    });
}

/// A DM ADDRESSES ITS OTHER SIDE: a post in a DM attributes the counterpart
/// like a mention, so an agent on the other side is summoned without being
/// @-named — and a post that does name them carries the one relation.
#[test]
fn a_dm_post_attributes_its_counterpart_as_a_mention() {
    block_on(async {
        let mut host = boot().await;
        apply(
            &mut host,
            key(1),
            "chat",
            ChatMsg::CreateDmChannel {
                counterpart: 2,
                name: "one & two".into(),
            },
        )
        .await;
        let dm = chat::client::dm_channel_id("1", "2");
        let plain = vec![Block::Paragraph(vec![Span {
            text: "hello".into(),
            marks: Vec::new(),
        }])];
        apply(
            &mut host,
            key(1),
            "chat",
            ChatMsg::PostMessage {
                channel_id: dm.clone(),
                message_id: "d1".into(),
                blocks: plain,
                thread: None,
            },
        )
        .await;
        let addressed = relations(&host, "chat", "message", "d1").await;
        assert_eq!(
            addressed
                .relations
                .iter()
                .map(|r| (r.recipient, &r.reason))
                .collect::<Vec<_>>(),
            vec![(1, &Reason::Authorship), (2, &Reason::Mention)]
        );
        // naming them outright is still one mention, not two
        apply(
            &mut host,
            key(1),
            "chat",
            ChatMsg::PostMessage {
                channel_id: dm,
                message_id: "d2".into(),
                blocks: body(2),
                thread: None,
            },
        )
        .await;
        let named = relations(&host, "chat", "message", "d2").await;
        assert_eq!(
            named.relations.iter().filter(|r| r.recipient == 2).count(),
            1
        );
    });
}
