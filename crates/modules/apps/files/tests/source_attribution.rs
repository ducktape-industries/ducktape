mod harness;

use attribution::{AttributionMsg, AttributionQuery, AttributionReply, Reason, Source};
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use files::{
    Actor, Change, Content, FilesMsg, FilesQuery, FilesReply, FilesWriteOutput, WriteOutcome,
};
use futures::executor::block_on;
use host::{BlockContext, BlockOutcome, Host};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot};

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
        // Identity's creation callback has no further action in this test.
        let Ok(operations) = serde_json::from_slice::<Vec<FilesMsg>>(&msg.payload) else {
            return Ok(());
        };
        for operation in operations {
            ctx.emit_msg(file_msg(operation));
        }
        Ok(())
    }
}

#[derive(Default)]
struct Stores {
    identity: harness::SharedStore,
    attribution: harness::SharedStore,
}
fn arena(dir: &tempfile::TempDir, stores: &Stores) -> Host {
    Host::genesis(vec![
        Box::new(files::Files::open("files", dir.path().into()).unwrap()),
        Box::new(identity_module::Identity::new(
            "identity",
            Box::new(stores.identity.clone()),
            "files-test".into(),
        )),
        Box::new(attribution_module::AttributionModule::new(
            "attribution",
            Box::new(stores.attribution.clone()),
        )),
        Box::new(Executor),
    ])
    .unwrap()
}
fn context(origin: Origin) -> BlockContext {
    BlockContext {
        height: 1,
        consensus_time: 1,
        origin,
    }
}
fn alice_key() -> PrivateKey {
    PrivateKey::from_seed(41)
}
fn alice() -> Origin {
    Origin::External(alice_key().public_key().as_ref().to_vec())
}
fn file_msg(operation: FilesMsg) -> Msg {
    Msg {
        target: "files".into(),
        payload: files::encode_msg(&operation),
    }
}
fn write(path: &str) -> FilesMsg {
    FilesMsg::Commit {
        base_snapshot: None,
        message: "write".into(),
        changes: vec![Change::Put {
            path: path.into(),
            exec: false,
            meta: Default::default(),
            content: Content::Inline { b64: "aGk=".into() },
        }],
    }
}
async fn identity(host: &mut Host, origin: Origin, operation: identity::IdentityMsg) {
    host.submit_at(
        context(origin),
        Msg {
            target: "identity".into(),
            payload: identity::encode_msg(&operation),
        },
    )
    .await
    .unwrap();
}
async fn provision(host: &mut Host) {
    identity(
        host,
        alice(),
        identity::IdentityMsg::Create {
            name: "Alice".into(),
            scheme: identity::KeyScheme::Ed25519,
        },
    )
    .await;
    identity(
        host,
        Origin::Module("executor".into()),
        identity::IdentityMsg::CreateProgram {
            name: "Writer".into(),
            controller: 1,
            request: 0,
        },
    )
    .await;
}
async fn query(host: &Host, query: FilesQuery) -> FilesReply {
    files::decode_reply(
        &host
            .query("files", &files::encode_query(&query))
            .await
            .unwrap(),
    )
    .unwrap()
}
async fn relations(host: &Host, kind: &str, object: String) -> attribution::ObjectRelations {
    let query = AttributionQuery::Relations {
        source: Source {
            module: "files".into(),
            kind: kind.into(),
            object,
        },
    };
    let reply = host
        .query("attribution", &attribution::encode_query(&query))
        .await
        .unwrap();
    let AttributionReply::Relations(Some(relations)) = attribution::decode_reply(&reply).unwrap()
    else {
        panic!("source record")
    };
    relations
}
fn output(outcome: &BlockOutcome) -> FilesWriteOutput {
    let record = outcome
        .dispatches
        .iter()
        .find(|record| record.module == "files")
        .unwrap();
    files::decode_write_output(record.output.as_ref().expect("files result declared")).unwrap()
}
fn committed_snapshot(output: &FilesWriteOutput) -> String {
    let WriteOutcome::Commit { snapshot } = &output.outcome else {
        panic!("commit result")
    };
    snapshot.clone()
}

#[test]
fn program_is_the_real_author_and_pin_owner_and_recreation_survives_disk_reopen() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let stores = Stores::default();
        let mut host = arena(&dir, &stores);
        provision(&mut host).await;
        let outcome = host
            .submit_at(
                context(Origin::Program(2)),
                file_msg(write("/home/acct:2/result")),
            )
            .await
            .unwrap();
        let result = output(&outcome);
        assert_eq!(result.actor, Actor::Account(2));
        assert_eq!(result.source_revision, 1);
        let snapshot = committed_snapshot(&result);
        let source = relations(&host, "snapshot", snapshot.clone()).await;
        assert_eq!(source.relations.len(), 1);
        assert_eq!(source.relations[0].recipient, 2);
        assert_eq!(source.relations[0].reason, Reason::Authorship);
        let FilesReply::History(history) = query(&host, FilesQuery::History { limit: 10 }).await
        else {
            panic!("history")
        };
        assert_eq!(history[0].author, Actor::Account(2));
        assert_eq!(history[0].id, snapshot);

        // Separator and NUL are valid pin-name bytes; the attribution source ID
        // encodes those bytes rather than imposing another name restriction.
        let name = "release\0:|".to_string();
        let pin = FilesMsg::Pin {
            snapshot: snapshot.clone(),
            name: name.clone(),
        };
        host.submit_at(context(Origin::Program(2)), file_msg(pin.clone()))
            .await
            .unwrap();
        let source_id = files::to_hex(name.as_bytes());
        let source = relations(&host, "pin", source_id.clone()).await;
        assert_eq!(source.revision, 2);
        assert_eq!(source.relations[0].recipient, 2);
        assert_eq!(source.relations[0].reason, Reason::Ownership);
        host.submit_at(
            context(Origin::Program(2)),
            file_msg(FilesMsg::Unpin { name: name.clone() }),
        )
        .await
        .unwrap();
        let removed = relations(&host, "pin", source_id.clone()).await;
        assert_eq!(removed.revision, 3);
        assert!(removed.relations.is_empty());
        let root = host.root_hash();
        drop(host);
        let mut reloaded = arena(&dir, &stores);
        assert_eq!(reloaded.root_hash(), root);
        reloaded
            .submit_at(context(Origin::Program(2)), file_msg(pin))
            .await
            .unwrap();
        let recreated = relations(&reloaded, "pin", source_id).await;
        assert_eq!(recreated.revision, 4);
        assert_eq!(recreated.changes, 3);
    });
}

#[test]
fn same_unit_followups_allocate_distinct_revisions_before_attributions_drain() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let mut host = arena(&dir, &Stores::default());
        let written = host
            .submit_at(context(Origin::System), file_msg(write("/shared/file")))
            .await
            .unwrap();
        let snapshot = committed_snapshot(&output(&written));
        let pin = FilesMsg::Pin {
            snapshot,
            name: "same".into(),
        };
        let operations = vec![
            pin.clone(),
            FilesMsg::Unpin {
                name: "same".into(),
            },
            pin,
        ];
        let outcome = host
            .submit_at(
                context(alice()),
                Msg {
                    target: "executor".into(),
                    payload: serde_json::to_vec(&operations).unwrap(),
                },
            )
            .await
            .unwrap();
        let revisions: Vec<_> = outcome
            .dispatches
            .iter()
            .filter(|record| record.module == "files")
            .map(|record| {
                files::decode_write_output(record.output.as_ref().unwrap())
                    .unwrap()
                    .source_revision
            })
            .collect();
        assert_eq!(revisions, [2, 3, 4]);
        let source = relations(&host, "pin", files::to_hex(b"same")).await;
        assert_eq!(source.revision, 4);
        assert!(
            source.relations.is_empty(),
            "module ownership creates no invented account"
        );
    });
}

#[test]
fn unauthorized_or_suspended_program_writes_and_failed_publications_leave_no_source_effects() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let mut host = arena(&dir, &Stores::default());
        provision(&mut host).await;
        let before = host.root_hash();
        assert!(
            host.submit_at(context(Origin::Program(2)), file_msg(write("/etc/private")))
                .await
                .is_err()
        );
        assert_eq!(host.root_hash(), before);
        let result = host
            .submit_at(context(Origin::Program(2)), file_msg(write("/shared/good")))
            .await
            .unwrap();
        let snapshot = committed_snapshot(&output(&result));
        host.submit_at(
            context(Origin::Module("files".into())),
            Msg {
                target: "attribution".into(),
                payload: attribution::encode_msg(&AttributionMsg::Attribute {
                    object: attribution::ObjectRef {
                        kind: "pin".into(),
                        object: files::to_hex(b"conflict"),
                    },
                    revision: 100,
                    actor: attribution::Actor::System,
                    relations: Vec::new(),
                    transfers: Vec::new(),
                }),
            },
        )
        .await
        .unwrap();
        let before = host.root_hash();
        assert!(
            host.submit_at(
                context(Origin::Program(2)),
                file_msg(FilesMsg::Pin {
                    snapshot,
                    name: "conflict".into()
                })
            )
            .await
            .is_err()
        );
        assert_eq!(
            host.root_hash(),
            before,
            "source refs and rejected publication roll back together"
        );
        let FilesReply::Refs(refs) = query(&host, FilesQuery::Refs {}).await else {
            panic!("refs")
        };
        assert!(refs.pins.is_empty());
        let result = host
            .submit_at(
                context(Origin::Program(2)),
                file_msg(write("/shared/after")),
            )
            .await
            .unwrap();
        assert_eq!(
            output(&result).source_revision,
            2,
            "failed source did not consume a revision"
        );
        identity(
            &mut host,
            Origin::Module("executor".into()),
            identity::IdentityMsg::SetProgramStanding {
                account: 2,
                standing: identity::ProgramStanding::Suspended,
            },
        )
        .await;
        let before = host.root_hash();
        assert!(
            host.submit_at(
                context(Origin::Program(2)),
                file_msg(write("/shared/suspended"))
            )
            .await
            .is_err()
        );
        assert_eq!(host.root_hash(), before);
    });
}

#[test]
fn a_keys_writes_are_attributed_to_the_account_it_holds_at_the_time() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let mut host = arena(&dir, &Stores::default());
        let key = alice_key();
        let key_bytes = key.public_key().as_ref().to_vec();
        let home = format!("/home/ext:{}/file", files::to_hex(&key_bytes));
        let written = host
            .submit_at(context(alice()), file_msg(write(&home)))
            .await
            .unwrap();
        assert_eq!(output(&written).actor, Actor::Key(key_bytes.clone()));
        let pin = FilesMsg::Pin {
            snapshot: committed_snapshot(&output(&written)),
            name: "old".into(),
        };
        host.submit_at(context(alice()), file_msg(pin))
            .await
            .unwrap();
        identity(
            &mut host,
            alice(),
            identity::IdentityMsg::Create {
                name: "Alice".into(),
                scheme: identity::KeyScheme::Ed25519,
            },
        )
        .await;
        let written = host
            .submit_at(
                context(alice()),
                file_msg(write(&format!("{home}.admitted"))),
            )
            .await
            .unwrap();
        assert_eq!(output(&written).actor, Actor::Account(1));
        let sibling = PrivateKey::from_seed(42);
        let sibling_bytes = sibling.public_key().as_ref().to_vec();
        let preimage = identity::add_key_preimage(
            "files-test",
            identity::KeyScheme::Ed25519,
            &sibling_bytes,
            0,
            1,
            100,
        );
        identity(
            &mut host,
            Origin::External(sibling_bytes.clone()),
            identity::IdentityMsg::AddKey {
                scheme: identity::KeyScheme::Ed25519,
                label: None,
                authorizer: identity::Authorizer {
                    key: key_bytes.clone(),
                    account: 1,
                    expires_at: 100,
                    proof: key
                        .sign(identity::IDENTITY_ADD_KEY_NS, &preimage)
                        .as_ref()
                        .to_vec(),
                },
            },
        )
        .await;
        // Alice can remove herself while the admitted sibling keeps account 1.
        identity(
            &mut host,
            alice(),
            identity::IdentityMsg::RemoveKey { key: key_bytes },
        )
        .await;
        identity(
            &mut host,
            alice(),
            identity::IdentityMsg::Create {
                name: "Reassigned".into(),
                scheme: identity::KeyScheme::Ed25519,
            },
        )
        .await;
        let written = host
            .submit_at(
                context(alice()),
                file_msg(write(&format!("{home}.reassigned"))),
            )
            .await
            .unwrap();
        assert_eq!(output(&written).actor, Actor::Account(2));
        host.submit_at(
            context(alice()),
            file_msg(FilesMsg::Unpin { name: "old".into() }),
        )
        .await
        .unwrap();
    });
}
