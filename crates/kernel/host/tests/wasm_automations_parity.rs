//! the STORE-BACKED cutover-continuity proof for automations: the
//! `automations` guest component over `WasmModule::with_store(QmdbStore)` and
//! the native `Automations` over the same store shape are ROOT-CONTINUOUS —
//! the same op sequence commits the IDENTICAL qmdb merkle root after every
//! block (both roots ARE the store's root; qmdb's batch canonicalizes
//! mutations by hashed key, so the native logical-key commit order and the
//! wasm hashed-key drain order produce the same op log), including the
//! byte-identical NO-OP blocks the idempotent SetEnabled relies on.
//!
//! automations is a chat-HOOK SUBSCRIBER: both hosts carry the REAL native
//! siblings under the production ids (`bin/node/src/host_state.rs` —
//! chat over its own qmdb store, tasks, identity, attribution, inbox) and drive actual chat posts
//! through the hook fan-out, so on the wasm side the whole intake runs in the
//! guest: the origin gate, the loop-prevention author check, the text fetch
//! and the pre-emit PROBES (channel-exists / message-id-unused /
//! task-id-unused — host-routed `query-module` reads resolved through
//! memoized replay against the live siblings), and the follow-up emissions
//! that land on chat/tasks/attribution in the triggering block. Inbox consumes
//! the committed attribution changes in later units.
//! the NO-FAIL contract crosses the seam unchanged: a probe rejection stages
//! a RunRecord instead of aborting the posting user's block.

use automations::{
    Action, Automations, AutomationsMsg, AutomationsQuery, AutomationsReply, MAX_FILTER_BYTES,
    MAX_ID_BYTES, MAX_TEMPLATE_BYTES, Trigger, decode_reply, encode_msg, encode_query,
};
use chat::{Block, Chat, ChatMsg, PostPolicy, encode_msg as chat_encode_msg};
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::{BlockContext, Host, MemberOutcome, SubmitError};
use inbox::Inbox;
use sdk::{Error, Module, Msg, Origin, StateRoot};
use statesync::qmdb::QmdbStore;
use tasks::{
    TaskMsg, TaskQuery, TaskReply, Tasks, decode_task_reply as tasks_decode_reply,
    encode_task_msg as tasks_encode_msg, encode_task_query as tasks_encode_query,
};
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `automations` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const AUTOMATIONS_WASM: &[u8] = include_bytes!("fixtures/automations.component.wasm");

fn wasm_automations(store: Box<dyn sdk::MerkleStore>) -> WasmModule {
    // NOTE: no sibling wiring here — the guest compiles the exact production
    // builder chain (`Automations::new(.., "chat", "tasks", "identity", "attribution")`) in;
    // only the store arrives host-constructed.
    WasmModule::with_store("automations", AUTOMATIONS_WASM, store).expect("load component")
}

/// the production wiring, verbatim (`bin/node/src/host_state.rs`).
fn native_automations(store: Box<dyn sdk::MerkleStore>) -> Automations {
    Automations::new(
        "automations",
        store,
        "chat",
        "tasks",
        "identity",
        "attribution",
    )
}

/// a 32-byte submitter key (the ordered lane hands modules verified ed25519
/// ids; the parity claim only needs them distinct and non-empty).
fn key(tag: u8) -> Vec<u8> {
    PrivateKey::from_seed(u64::from(tag))
        .public_key()
        .as_ref()
        .to_vec()
}

async fn identity_fixture() -> identity::Identity {
    let mut identity = identity::Identity::new(
        "identity",
        Box::new(sdk_testkit::MemStore::new()),
        "parity".into(),
    );
    for tag in [0xA1, 0xB2, 0xC3] {
        let mut ctx = sdk_testkit::TestCtx::with_env(sdk::Env {
            height: 0,
            consensus_time: 0,
            me: "identity".into(),
            origin: Origin::External(key(tag)),
            cause: sdk::Cause::Direct,
        });
        identity
            .execute(
                &mut ctx,
                &Msg {
                    target: "identity".into(),
                    payload: identity::encode_msg(&identity::IdentityMsg::Create {
                        name: format!("member-{tag}"),
                        scheme: identity::KeyScheme::Ed25519,
                    }),
                },
            )
            .await
            .unwrap();
    }
    identity.commit_block().await.unwrap();
    identity
}

/// the shared native sibling set: chat over a REAL qmdb store (the module the
/// hook events and probes run against), tasks, identity, attribution, inbox.
async fn siblings(
    context: &deterministic::Context,
    label: &'static str,
) -> Vec<Box<dyn sdk::Module>> {
    let store = QmdbStore::init(context.child(label), "chat").await;
    vec![
        Box::new(
            Chat::new("chat", Box::new(store))
                .with_identity("identity")
                .with_attribution("attribution"),
        ),
        Box::new(identity_fixture().await),
        Box::new(
            attribution::AttributionModule::new(
                "attribution",
                Box::new(sdk_testkit::MemStore::new()),
            )
            .with_subscribers(["inbox"]),
        ),
        Box::new(Tasks::new(
            "tasks",
            "identity",
            "attribution",
            Box::new(sdk_testkit::MemStore::new()),
        )),
        Box::new(Inbox::new(
            "inbox",
            Box::new(sdk_testkit::MemStore::new()),
            "attribution",
            "identity",
        )),
    ]
}

async fn native_host(context: &deterministic::Context) -> Host {
    let mut modules = siblings(context, "native_chat").await;
    let store = QmdbStore::init(context.child("native_auto"), "automations").await;
    modules.push(Box::new(native_automations(Box::new(store))));
    Host::genesis(modules).expect("genesis")
}

async fn wasm_host_(context: &deterministic::Context) -> Host {
    let mut modules = siblings(context, "wasm_chat").await;
    let store = QmdbStore::init(context.child("wasm_auto"), "automations").await;
    modules.push(Box::new(wasm_automations(Box::new(store))));
    Host::genesis(modules).expect("genesis")
}

fn block(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: 1_000 + height,
        origin,
    }
}

fn auto_op(m: &AutomationsMsg) -> Msg {
    Msg {
        target: "automations".into(),
        payload: encode_msg(m),
    }
}

fn create_rule(rule_id: &str, trigger: Trigger, action: Action) -> Msg {
    auto_op(&AutomationsMsg::CreateRule {
        rule_id: rule_id.into(),
        trigger,
        action,
    })
}

fn on_general(text_contains: Option<&str>) -> Trigger {
    Trigger {
        channel_id: Some("general".into()),
        mention: None,
        text_contains: text_contains.map(Into::into),
    }
}

fn post(channel: &str, id: &str, text: &str) -> Msg {
    Msg {
        target: "chat".into(),
        payload: chat_encode_msg(&ChatMsg::PostMessage {
            channel_id: channel.into(),
            message_id: id.into(),
            blocks: vec![Block::paragraph(text)],
            thread: None,
        }),
    }
}

const RULE_IDS: [&str; 4] = ["r-ghost", "r-inbox", "r-post", "r-task"];

/// the read matrix: the rule listing (fire counts ride in it), per-rule gets
/// (present and absent), and each rule's run history.
async fn replies(h: &Host) -> Vec<Vec<u8>> {
    let mut queries = vec![encode_query(&AutomationsQuery::ListRules)];
    for id in RULE_IDS {
        queries.push(encode_query(&AutomationsQuery::GetRule {
            rule_id: id.into(),
        }));
        queries.push(encode_query(&AutomationsQuery::RunHistory {
            rule_id: id.into(),
            limit: 64,
        }));
    }
    queries.push(encode_query(&AutomationsQuery::GetRule {
        rule_id: "absent".into(),
    }));
    let mut out = Vec::new();
    for q in &queries {
        out.push(h.query("automations", q).await.expect("query"));
    }
    out
}

fn root_of(h: &Host) -> StateRoot {
    h.module_root("automations")
        .expect("automations registered")
}

const SIBLING_IDS: [&str; 3] = ["chat", "tasks", "inbox"];

/// submit one ACCEPTED op to both hosts: IDENTICAL automations roots (both
/// are the store's merkle root), identical replies, per-sibling cross-host
/// agreement (the follow-up lanes — a diverging or missing
/// PostMessage/CreateTask/report diverges the sibling roots), lockstep
/// root movement.
async fn roundtrip(
    native: &mut Host,
    wasm: &mut Host,
    height: u64,
    origin: Origin,
    m: Msg,
    moves: bool,
) {
    let (n_before, w_before) = (root_of(native), root_of(wasm));
    native
        .submit_at(block(height, origin.clone()), m.clone())
        .await
        .expect("native submit");
    wasm.submit_at(block(height, origin), m)
        .await
        .expect("wasm submit");
    assert_eq!(
        root_of(native),
        root_of(wasm),
        "roots diverge after block {height}"
    );
    assert_eq!(
        replies(native).await,
        replies(wasm).await,
        "replies diverge after block {height}"
    );
    for sibling in SIBLING_IDS {
        assert_eq!(
            native.module_root(sibling),
            wasm.module_root(sibling),
            "the {sibling} sibling diverged at {height}"
        );
    }
    if moves {
        assert_ne!(root_of(native), n_before, "native root stuck at {height}");
        assert_ne!(root_of(wasm), w_before, "wasm root stuck at {height}");
    } else {
        assert_eq!(root_of(native), n_before, "native root moved at {height}");
        assert_eq!(root_of(wasm), w_before, "wasm root moved at {height}");
    }
}

/// submit one REJECTED op to both hosts: reasons carry the same needle, and
/// both automations roots (and all siblings) are byte-identical to pre-block.
async fn reject_roundtrip(
    native: &mut Host,
    wasm: &mut Host,
    height: u64,
    origin: Origin,
    m: Msg,
    needle: &str,
) {
    let (n_before, w_before) = (root_of(native), root_of(wasm));
    let n_err = native
        .submit_at(block(height, origin.clone()), m.clone())
        .await
        .expect_err("native must reject");
    let w_err = wasm
        .submit_at(block(height, origin), m)
        .await
        .expect_err("wasm must reject");
    let SubmitError::Rejected(Error::Module(n_msg)) = n_err else {
        panic!("native rejection shape: {n_err:?}");
    };
    let SubmitError::Rejected(Error::Module(w_msg)) = w_err else {
        panic!("wasm rejection shape: {w_err:?}");
    };
    assert!(n_msg.contains(needle), "native reason: {n_msg}");
    assert!(
        w_msg.contains(needle),
        "wasm reason must carry the native reason: {w_msg}"
    );
    assert_eq!(root_of(native), n_before, "native root moved on reject");
    assert_eq!(root_of(wasm), w_before, "wasm root moved on reject");
    for sibling in SIBLING_IDS {
        assert_eq!(native.module_root(sibling), wasm.module_root(sibling));
    }
    assert_eq!(replies(native).await, replies(wasm).await);
}

/// A rule's run history records the account its Report action addressed.
async fn run_details(h: &Host, rule_id: &str) -> Vec<String> {
    let reply = h
        .query(
            "automations",
            &encode_query(&AutomationsQuery::RunHistory {
                rule_id: rule_id.into(),
                limit: 64,
            }),
        )
        .await
        .expect("history query");
    let AutomationsReply::History(records) = decode_reply(&reply).expect("decode") else {
        panic!("expected a history reply");
    };
    records.into_iter().map(|r| r.detail).collect()
}

/// decoded task list off one host (cross-host equality is separately pinned
/// via the sibling roots; this is for spot checks).
async fn task_ids(h: &Host) -> Vec<String> {
    let reply = h
        .query(
            "tasks",
            &tasks_encode_query(&TaskQuery::List {
                limit: tasks::MAX_LIST_LIMIT,
                after: None,
            }),
        )
        .await
        .expect("tasks query");
    let TaskReply::Tasks(tasks) = tasks_decode_reply(&reply).expect("decode") else {
        panic!("a list answers a page");
    };
    tasks.into_iter().map(|t| t.id).collect()
}

#[test]
fn same_ops_same_replies_follow_ups_land_and_probes_downgrade() {
    deterministic::Runner::default().start(|context| async move {
        let mut native = native_host(&context).await;
        let mut wasm = wasm_host_(&context).await;
        let user = Origin::External(key(0xA1));
        let ops = Origin::External(key(0xB2));

        // ROOT CONTINUITY from block zero: both sides commit to the SAME
        // (empty) qmdb store — equal roots, and both are per-block durable on
        // it, so recovery's disk cohort holds them on either side.
        assert_ne!(root_of(&native), StateRoot::ZERO);
        assert_eq!(root_of(&native), root_of(&wasm), "genesis roots diverge");
        assert!(native.block_durable_ids().contains("automations"));
        assert!(wasm.block_durable_ids().contains("automations"));
        // The inbox sibling's empty root, for the later delivery checks.
        let inbox_genesis = native.module_root("inbox").expect("inbox registered");

        // ---- the shared world: a hooked channel. sibling-only blocks leave
        // automations alone on both runtimes.
        let (n0, w0) = (root_of(&native), root_of(&wasm));
        for host in [&mut native, &mut wasm] {
            host.submit_at(
                block(1, user.clone()),
                Msg {
                    target: "chat".into(),
                    payload: chat_encode_msg(&ChatMsg::CreateChannel {
                        channel_id: "general".into(),
                        name: "General".into(),
                        post_policy: PostPolicy::Open,
                    }),
                },
            )
            .await
            .expect("create channel");
            host.submit_at(
                block(2, user.clone()),
                Msg {
                    target: "chat".into(),
                    payload: chat_encode_msg(&ChatMsg::RegisterHook {
                        channel_id: "general".into(),
                        module_id: "automations".into(),
                    }),
                },
            )
            .await
            .expect("register hook");
        }
        assert_eq!(root_of(&native), n0, "sibling blocks hold the native root");
        assert_eq!(root_of(&wasm), w0, "sibling blocks hold the wasm root");

        // ---- rule CRUD: every action family, one rule each. r-ghost targets
        // a channel that does not exist — its every fire must DOWNGRADE to a
        // RunRecord through the probe layer, never abort a posting block.
        roundtrip(
            &mut native,
            &mut wasm,
            3,
            ops.clone(),
            create_rule(
                "r-post",
                on_general(Some("duck")),
                Action::PostMessage {
                    channel_id: "general".into(),
                    template: "echo {author}: {text}".into(),
                },
            ),
            true,
        )
        .await;
        roundtrip(
            &mut native,
            &mut wasm,
            4,
            ops.clone(),
            create_rule(
                "r-task",
                on_general(None),
                Action::CreateTask {
                    task_id_prefix: "job".into(),
                    title_template: "follow up on {channel}#{seq}".into(),
                },
            ),
            true,
        )
        .await;
        roundtrip(
            &mut native,
            &mut wasm,
            5,
            ops.clone(),
            create_rule(
                "r-inbox",
                on_general(None),
                Action::Report {
                    recipient: 2,
                    kind: "note".into(),
                    body_template: "{author} said: {text}".into(),
                },
            ),
            true,
        )
        .await;
        roundtrip(
            &mut native,
            &mut wasm,
            6,
            ops.clone(),
            create_rule(
                "r-ghost",
                on_general(None),
                Action::PostMessage {
                    channel_id: "missing".into(),
                    template: "into the void".into(),
                },
            ),
            true,
        )
        .await;

        // ---- the first user post (seq 1): the hook fans the event into the
        // guest, where the whole intake runs — r-ghost downgrades (channel
        // probe), r-inbox delivers, r-post passes both chat probes and posts
        // (its module-authored reply is seq 2, whose OWN hook event must be
        // loop-prevented), r-task creates job-r-task-general-1. everything commits
        // in the posting block, identically.
        roundtrip(
            &mut native,
            &mut wasm,
            7,
            user.clone(),
            post("general", "m1", "the duck says hi"),
            true,
        )
        .await;
        assert_eq!(task_ids(&wasm).await, vec!["job-r-task-general-1".to_string()]);
        // Reports are source-owned attribution changes in this block; inbox
        // ingestion is a later unit. Its queue may already contain channel and
        // rule ownership changes, so compare the report itself here.
        let bytes = wasm
            .query(
                "attribution",
                &attribution::encode_query(&attribution::AttributionQuery::ChangesFor {
                    recipient: 2,
                    after: 0,
                    limit: 64,
                }),
            )
            .await
            .unwrap();
        let attribution::AttributionReply::Changes(changes) =
            attribution::decode_reply(&bytes).unwrap()
        else {
            panic!("change list");
        };
        assert!(
            changes
                .iter()
                .any(|entry| entry.change.source.module == "automations"
                    && entry.change.source.kind == "report"
                    && entry.change.reason == attribution::Reason::Report
                    && entry.change.height == 7)
        );
        assert_eq!(
            run_details(&wasm, "r-inbox").await,
            vec!["reported note to account 2".to_string()],
            "the report must name the rule owner account"
        );

        // ---- a post WITHOUT the text filter's needle: r-post skips (its
        // fire_count must not bump), the unfiltered rules still fire.
        roundtrip(
            &mut native,
            &mut wasm,
            8,
            user.clone(),
            post("general", "m2", "no waterfowl here"),
            true,
        )
        .await;

        // ---- SetEnabled false: a disabled rule never engages.
        roundtrip(
            &mut native,
            &mut wasm,
            9,
            ops.clone(),
            auto_op(&AutomationsMsg::SetEnabled {
                rule_id: "r-task".into(),
                enabled: false,
            }),
            true,
        )
        .await;
        roundtrip(
            &mut native,
            &mut wasm,
            10,
            user.clone(),
            post("general", "m3", "duck again"),
            true,
        )
        .await;
        assert_eq!(
            task_ids(&wasm).await,
            vec![
                "job-r-task-general-1".to_string(),
                "job-r-task-general-3".to_string()
            ],
            "the disabled window skipped exactly the middle post"
        );

        // ---- id squatting: pre-create the task id the NEXT post composes
        // (m3 was seq 4 + r-post's reply seq 5 → the next user post is seq 6),
        // re-enable r-task, and post — the task probe must downgrade the fire
        // to a RunRecord on both runtimes instead of aborting the block.
        for host in [&mut native, &mut wasm] {
            host.submit_at(
                block(11, ops.clone()),
                Msg {
                    target: "tasks".into(),
                    payload: tasks_encode_msg(&TaskMsg::CreateTask {
                        task_id: "job-r-task-general-6".into(),
                        title: "squatted".into(),
                        owner: None,
                    }),
                },
            )
            .await
            .expect("squat the composed id");
        }
        roundtrip(
            &mut native,
            &mut wasm,
            12,
            ops.clone(),
            auto_op(&AutomationsMsg::SetEnabled {
                rule_id: "r-task".into(),
                enabled: true,
            }),
            true,
        )
        .await;
        roundtrip(
            &mut native,
            &mut wasm,
            13,
            user.clone(),
            post("general", "m4", "quiet post"),
            true,
        )
        .await;
        // no NEW task landed (the squat is the only "job-r-task-general-6").
        assert_eq!(
            task_ids(&wasm).await.len(),
            3,
            "the probe downgraded the fire"
        );

        // ---- DeleteRule: the tombstone hides the rule from reads.
        roundtrip(
            &mut native,
            &mut wasm,
            14,
            ops.clone(),
            auto_op(&AutomationsMsg::DeleteRule {
                rule_id: "r-ghost".into(),
            }),
            true,
        )
        .await;
        let reply = wasm
            .query(
                "automations",
                &encode_query(&AutomationsQuery::GetRule {
                    rule_id: "r-ghost".into(),
                }),
            )
            .await
            .expect("get");
        assert_eq!(
            decode_reply(&reply).expect("decode"),
            AutomationsReply::Rule(None)
        );

        // Deferred attribution delivery changed both inbox roots identically.
        assert_eq!(
            native.module_root("inbox"),
            wasm.module_root("inbox"),
            "the inbox deliveries diverged between the runtimes"
        );
        assert_ne!(
            native.module_root("inbox").expect("inbox registered"),
            inbox_genesis,
            "the hooked posts delivered nothing to the inbox"
        );

        // queries are read-only on the wasm side too.
        let settled = root_of(&wasm);
        let _ = replies(&wasm).await;
        assert_eq!(root_of(&wasm), settled, "a query moved the wasm root");
    });
}

#[test]
fn rejections_match_and_leave_no_trace() {
    deterministic::Runner::default().start(|context| async move {
        let mut native = native_host(&context).await;
        let mut wasm = wasm_host_(&context).await;
        let ops = Origin::External(key(0xB2));

        for host in [&mut native, &mut wasm] {
            host.submit_at(
                block(1, ops.clone()),
                create_rule(
                    "r-post",
                    on_general(None),
                    Action::PostMessage {
                        channel_id: "general".into(),
                        template: "x".into(),
                    },
                ),
            )
            .await
            .expect("seed rule");
        }

        // every distinct refusal family: id/filter/template caps, action
        // shapes, duplicates, unknowns, the forged-hook origin gate, and the
        // decode seam. each rejected block leaves BOTH roots byte-identical.
        let rejects: Vec<(Msg, &str)> = vec![
            (
                create_rule(
                    "",
                    on_general(None),
                    Action::PostMessage {
                        channel_id: "c".into(),
                        template: "t".into(),
                    },
                ),
                "rule_id must not be empty",
            ),
            (
                create_rule(
                    &"r".repeat(MAX_ID_BYTES + 1),
                    on_general(None),
                    Action::PostMessage {
                        channel_id: "c".into(),
                        template: "t".into(),
                    },
                ),
                "rule_id exceeds",
            ),
            (
                create_rule(
                    "r1",
                    Trigger {
                        channel_id: Some(String::new()),
                        mention: None,
                        text_contains: None,
                    },
                    Action::PostMessage {
                        channel_id: "c".into(),
                        template: "t".into(),
                    },
                ),
                "trigger channel_id must not be empty",
            ),
            (
                create_rule(
                    "r1",
                    Trigger {
                        channel_id: None,
                        mention: Some("m".repeat(MAX_FILTER_BYTES + 1)),
                        text_contains: None,
                    },
                    Action::PostMessage {
                        channel_id: "c".into(),
                        template: "t".into(),
                    },
                ),
                "trigger mention exceeds",
            ),
            (
                create_rule(
                    "r1",
                    on_general(None),
                    Action::PostMessage {
                        channel_id: String::new(),
                        template: "t".into(),
                    },
                ),
                "action channel_id must not be empty",
            ),
            (
                create_rule(
                    "r1",
                    on_general(None),
                    Action::PostMessage {
                        channel_id: "c".into(),
                        template: "t".repeat(MAX_TEMPLATE_BYTES + 1),
                    },
                ),
                "action template exceeds",
            ),
            (
                create_rule(
                    "r1",
                    on_general(None),
                    Action::CreateTask {
                        task_id_prefix: String::new(),
                        title_template: "t".into(),
                    },
                ),
                "action task_id_prefix must not be empty",
            ),
            (
                create_rule(
                    "r1",
                    on_general(None),
                    Action::Report {
                        recipient: 2,
                        kind: String::new(),
                        body_template: "b".into(),
                    },
                ),
                "action kind must not be empty",
            ),
            (
                create_rule(
                    "r-post",
                    on_general(None),
                    Action::PostMessage {
                        channel_id: "c".into(),
                        template: "t".into(),
                    },
                ),
                "rule already exists",
            ),
            (
                auto_op(&AutomationsMsg::SetEnabled {
                    rule_id: "ghost".into(),
                    enabled: true,
                }),
                "unknown rule",
            ),
            (
                auto_op(&AutomationsMsg::DeleteRule {
                    rule_id: "ghost".into(),
                }),
                "unknown rule",
            ),
            // the spoof gate: a SUBMITTED HookEvent (any non-chat origin)
            // rejects — only chat's own follow-ups wear the chat origin.
            (
                auto_op(&AutomationsMsg::HookEvent(b"forged".to_vec())),
                "hook events must originate from the chat module",
            ),
            (
                Msg {
                    target: "automations".into(),
                    payload: b"definitely-not-json".to_vec(),
                },
                "expected value",
            ),
        ];

        for (height, (m, needle)) in rejects.into_iter().enumerate() {
            let height = height as u64 + 2;
            reject_roundtrip(&mut native, &mut wasm, height, ops.clone(), m, needle).await;
        }

        // the submitter resolution, proven in the compiled component and not
        // just natively: `ops` registered `r-post`, and any other account
        // administers it, while a module/system origin has no account to
        // record and mints no rule. `env().origin` is the one such input
        // that crosses the WIT boundary, so it is checked on both sides.
        let stranger = Origin::External(key(0xC3));
        let unownable = [
            (Origin::External(Vec::new()), "non-empty submitter id"),
            // a NON-chat module id: the chat origin is the hook lane, which is
            // routed before the submitter is resolved and never rejects by
            // design.
            (
                Origin::Module("governance".into()),
                "automation rules require an account origin",
            ),
            (Origin::System, "automation rules require an account origin"),
        ];
        let mut height = 100;
        for op in [
            auto_op(&AutomationsMsg::SetEnabled {
                rule_id: "r-post".into(),
                enabled: false,
            }),
            auto_op(&AutomationsMsg::DeleteRule {
                rule_id: "r-post".into(),
            }),
        ] {
            roundtrip(&mut native, &mut wasm, height, stranger.clone(), op, true).await;
            height += 1;
        }
        for (origin, needle) in unownable {
            reject_roundtrip(
                &mut native,
                &mut wasm,
                height,
                origin,
                create_rule(
                    "r-unowned",
                    on_general(None),
                    Action::CreateTask {
                        task_id_prefix: "p".into(),
                        title_template: "t".into(),
                    },
                ),
                needle,
            )
            .await;
            height += 1;
        }
    });
}

#[test]
fn multi_dispatch_block_reads_prior_writes_and_isolates_rejections() {
    deterministic::Runner::default().start(|context| async move {
        let mut native = native_host(&context).await;
        let mut wasm = wasm_host_(&context).await;
        let ops = Origin::External(key(0xB2));

        // ONE block, three ops: the disable reads the rule STAGED by the
        // first dispatch, and the delete reads the disable's staged record —
        // on the wasm side each later dispatch reloads the prior dispatch's
        // staged `__state` (the read-your-writes seam the adapter relies on).
        let batch = vec![
            (
                ops.clone(),
                create_rule(
                    "x",
                    on_general(None),
                    Action::CreateTask {
                        task_id_prefix: "p".into(),
                        title_template: "t".into(),
                    },
                ),
            ),
            (
                ops.clone(),
                auto_op(&AutomationsMsg::SetEnabled {
                    rule_id: "x".into(),
                    enabled: false,
                }),
            ),
            (
                ops.clone(),
                auto_op(&AutomationsMsg::DeleteRule {
                    rule_id: "x".into(),
                }),
            ),
        ];
        let n_out = native
            .submit_block(block(1, ops.clone()), batch.clone())
            .await
            .expect("native block");
        let w_out = wasm
            .submit_block(block(1, ops.clone()), batch)
            .await
            .expect("wasm block");
        for out in [&n_out, &w_out] {
            assert!(
                out.members
                    .iter()
                    .all(|m| matches!(m, MemberOutcome::Applied { .. })),
                "all members must apply: {:?}",
                out.members
            );
        }
        assert_eq!(replies(&native).await, replies(&wasm).await);
        // created, disabled, and deleted within one block: nothing remains.
        let reply = wasm
            .query(
                "automations",
                &encode_query(&AutomationsQuery::GetRule {
                    rule_id: "x".into(),
                }),
            )
            .await
            .expect("get");
        assert_eq!(
            decode_reply(&reply).expect("decode"),
            AutomationsReply::Rule(None)
        );

        // ONE block where the middle member rejects AGAINST THE STAGE (a
        // duplicate of the rule staged by the first member — read-your-writes
        // decides the rejection), while the third member disables that same
        // staged rule: [Applied, Rejected, Applied].
        let (n_before, w_before) = (root_of(&native), root_of(&wasm));
        let batch = vec![
            (
                ops.clone(),
                create_rule(
                    "y",
                    on_general(None),
                    Action::CreateTask {
                        task_id_prefix: "p".into(),
                        title_template: "one".into(),
                    },
                ),
            ),
            (
                ops.clone(),
                create_rule(
                    "y",
                    on_general(None),
                    Action::CreateTask {
                        task_id_prefix: "p".into(),
                        title_template: "two".into(),
                    },
                ),
            ),
            (
                ops.clone(),
                auto_op(&AutomationsMsg::SetEnabled {
                    rule_id: "y".into(),
                    enabled: false,
                }),
            ),
        ];
        let n_out = native
            .submit_block(block(2, ops.clone()), batch.clone())
            .await
            .expect("native block");
        let w_out = wasm
            .submit_block(block(2, ops.clone()), batch)
            .await
            .expect("wasm block");
        for out in [&n_out, &w_out] {
            assert!(matches!(out.members[0], MemberOutcome::Applied { .. }));
            assert!(
                matches!(out.members[1], MemberOutcome::Rejected { .. }),
                "the duplicate must reject against the SAME-BLOCK stage: {:?}",
                out.members
            );
            assert!(matches!(out.members[2], MemberOutcome::Applied { .. }));
        }
        assert_ne!(root_of(&native), n_before);
        assert_ne!(root_of(&wasm), w_before);
        assert_eq!(replies(&native).await, replies(&wasm).await);
    });
}
