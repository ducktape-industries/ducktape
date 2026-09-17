//! governance authorizes a module CODE swap the same way it authorizes a node
//! upgrade: a member-gated proposal + simple-majority tally, and on passing,
//! `handle_execute` emits the modreg op as a host-drained follow-up.
//!
//! these tests register the REAL code registry: what is pinned is the whole
//! authorization chain the live network runs — governance emits, the host
//! stamps `Origin::Module("governance")`, modreg's origin gate accepts, and
//! the pending swap LANDS in consensus state.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use futures::executor::block_on;
use governance::Governance;
use governance::{
    GovAction, GovMsg, GovQuery, GovReply, ProposalStatus, decode_reply as gov_decode,
    encode_msg as gov_encode, encode_query as gov_query,
};
use host::{BlockContext, Host, SubmitError};
use modules::{
    Modules, ModulesMsg, ModulesQuery, ModulesReply, decode_reply as modules_decode,
    encode_msg as modules_encode, encode_query as modules_query,
};
use sdk::{Error, Msg, Origin};
use sdk_testkit::MemStore;
use valset::Valset;

fn member_key(seed: u8) -> Vec<u8> {
    let seed = [seed; 32];
    PrivateKey::decode(&seed[..])
        .expect("any 32 bytes is a valid seed")
        .public_key()
        .as_ref()
        .to_vec()
}

fn hash(seed: u8) -> Vec<u8> {
    vec![seed; modules::CODE_HASH_LEN]
}

/// a host with valset (members 1,2) + governance wired to the REAL code
/// registry, and the `hello` module pre-registered (active code = hash(1)).
async fn gov_host_with_modreg() -> Host {
    let mut valset = Valset::new("valset", Box::new(MemStore::new()), "governance");
    valset.seed(member_key(1)).await.expect("seed valset");
    valset.seed(member_key(2)).await.expect("seed valset");
    valset.finish_seed().await.expect("seed valset");
    let mut host = Host::genesis(vec![
        Box::new(valset),
        Box::new(
            Governance::new(
                "governance",
                Box::new(MemStore::new()),
                "valset",
                "identity",
            )
            .with_code_registry("modules"),
        ),
        Box::new(Modules::new(
            "modules",
            Box::new(MemStore::new()),
            "valset",
            "governance",
        )),
    ])
    .expect("genesis");
    // genesis-bootstrap the swappable module's initial code (System origin).
    host.submit_at(
        BlockContext {
            height: 0,
            consensus_time: 0,
            origin: Origin::System,
        },
        Msg {
            target: "modules".into(),
            payload: modules_encode(&ModulesMsg::RegisterModule {
                module_id: "hello".into(),
                kind: modules::Kind::Module,
                code_hash: hash(1),
                lanes: Vec::new(),
            }),
        },
    )
    .await
    .expect("register hello");
    host
}

async fn submit_as(
    host: &mut Host,
    who: &[u8],
    at: u64,
    payload: Vec<u8>,
) -> Result<(), SubmitError> {
    host.submit_at(
        BlockContext {
            height: at,
            consensus_time: at,
            origin: Origin::External(who.to_vec()),
        },
        Msg {
            target: "governance".into(),
            payload,
        },
    )
    .await
    .map(|_| ())
}

async fn proposal_status(host: &Host, id: &str) -> Option<ProposalStatus> {
    let reply = host
        .query(
            "governance",
            &gov_query(&GovQuery::Proposal {
                proposal_id: id.into(),
            }),
        )
        .await
        .expect("gov query");
    match gov_decode(&reply).expect("decode") {
        GovReply::Proposal(p) => p.map(|v| v.status),
        _ => None,
    }
}

async fn hello_code(host: &Host) -> modules::ModuleCode {
    let reply = host
        .query("modules", &modules_query(&ModulesQuery::ModuleStatus))
        .await
        .expect("modreg status");
    match modules_decode(&reply).expect("decode") {
        ModulesReply::ModuleStatus { modules } => modules
            .into_iter()
            .find(|m| m.module_id == "hello")
            .expect("hello entry"),
        other => panic!("expected Status, got {other:?}"),
    }
}

/// propose an action at `base`, both members vote yes, Execute — the early
/// strict-majority path settles Passed in the same round.
async fn pass(host: &mut Host, base: u64, id: &str, action: GovAction) {
    let (m1, m2) = (member_key(1), member_key(2));
    submit_as(
        host,
        &m1,
        base,
        gov_encode(&GovMsg::Propose {
            proposal_id: id.into(),
            action,
            voting_period: 100,
        }),
    )
    .await
    .expect("propose");
    for (who, at) in [(&m1, base + 1), (&m2, base + 2)] {
        submit_as(
            host,
            who,
            at,
            gov_encode(&GovMsg::Vote {
                proposal_id: id.into(),
                approve: true,
            }),
        )
        .await
        .expect("vote");
    }
    submit_as(
        host,
        &m2,
        base + 3,
        gov_encode(&GovMsg::Execute {
            proposal_id: id.into(),
        }),
    )
    .await
    .expect("execute");
}

/// the headline: a passing UpdateModule lands the pending swap in the REAL
/// registry — the whole chain from ballot to consensus code state.
#[test]
fn a_passing_update_module_lands_a_pending_swap_in_the_registry() {
    block_on(async {
        let mut host = gov_host_with_modreg().await;
        pass(
            &mut host,
            1,
            "mod-1",
            GovAction::UpdateModule {
                name: "hello-replacement".into(),
                module_id: "hello".into(),
                activation_lead: 500,
                code_hash: hash(2),
            },
        )
        .await;

        assert_eq!(
            proposal_status(&host, "mod-1").await,
            Some(ProposalStatus::Passed)
        );
        let code = hello_code(&host).await;
        assert_eq!(code.active_code_hash, hash(1), "active untouched until H");
        let pending = code.pending.expect("pending swap landed");
        assert_eq!(pending.name, "hello-replacement");
        // activation_lead (500) is RELATIVE to the EXECUTE height (base 1,
        // `pass` executes at base+3=4): 4 + 500 = 504.
        assert_eq!(pending.activation_height, 504);
        assert_eq!(pending.code_hash, hash(2));
    });
}

/// regression for #1775: a ballot that outlasts a SHORT voting period must
/// still schedule cleanly when it finally executes, long after propose. an
/// absolute `activation_height` chosen at propose time would already be
/// behind `execute_height + MIN_SWAP_LEAD` by the time this executes,
/// permanently rejecting the Execute op — the lead is relative to the
/// EXECUTE height instead, so a long-running ballot is never doomed by its
/// own duration.
#[test]
fn a_ballot_that_outlasts_its_lead_still_schedules_at_execute_time() {
    block_on(async {
        let mut host = gov_host_with_modreg().await;
        let (m1, m2) = (member_key(1), member_key(2));
        // a short activation_lead (just above the door's floor) and a voting
        // period long enough that Execute lands far past what would have
        // been the original propose-time absolute height.
        submit_as(
            &mut host,
            &m1,
            1,
            gov_encode(&GovMsg::Propose {
                proposal_id: "mod-late".into(),
                action: GovAction::UpdateModule {
                    name: "hello-replacement".into(),
                    module_id: "hello".into(),
                    activation_lead: governance::MIN_ACTIVATION_LEAD,
                    code_hash: hash(2),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect("propose");
        for (who, at) in [(&m1, 2u64), (&m2, 3u64)] {
            submit_as(
                &mut host,
                who,
                at,
                gov_encode(&GovMsg::Vote {
                    proposal_id: "mod-late".into(),
                    approve: true,
                }),
            )
            .await
            .expect("vote");
        }
        // deadline is 1 + 100 = 101; execute long after — an absolute height
        // computed AT PROPOSE (height 1) would be ancient history by now.
        let execute_height = 5_000;
        submit_as(
            &mut host,
            &m2,
            execute_height,
            gov_encode(&GovMsg::Execute {
                proposal_id: "mod-late".into(),
            }),
        )
        .await
        .expect("a long-outstanding ballot still schedules cleanly at execute time");

        assert_eq!(
            proposal_status(&host, "mod-late").await,
            Some(ProposalStatus::Passed)
        );
        let pending = hello_code(&host)
            .await
            .pending
            .expect("pending swap landed despite the long ballot");
        assert_eq!(
            pending.activation_height,
            execute_height + governance::MIN_ACTIVATION_LEAD,
            "activation is relative to the EXECUTE height, not the propose height"
        );
    });
}

/// a passing CancelModuleUpdate clears the pending swap before its boundary.
#[test]
fn a_passing_cancel_module_update_clears_the_pending_swap() {
    block_on(async {
        let mut host = gov_host_with_modreg().await;
        pass(
            &mut host,
            1,
            "mod-1",
            GovAction::UpdateModule {
                name: "hello-replacement".into(),
                module_id: "hello".into(),
                activation_lead: 500,
                code_hash: hash(2),
            },
        )
        .await;
        assert!(hello_code(&host).await.pending.is_some());

        pass(
            &mut host,
            10,
            "mod-cancel",
            GovAction::CancelModuleUpdate {
                name: "hello-replacement".into(),
                module_id: "hello".into(),
            },
        )
        .await;
        let code = hello_code(&host).await;
        assert!(code.pending.is_none(), "cancel cleared the pending swap");
        assert_eq!(code.active_code_hash, hash(1), "active untouched");
    });
}

/// a follow-up the registry REFUSES (unregistered module) fails the Execute op
/// atomically — the staged Passed transition rolls back, the proposal stays Open.
#[test]
fn a_refused_schedule_fails_execute_atomically() {
    block_on(async {
        let mut host = gov_host_with_modreg().await;
        let (m1, m2) = (member_key(1), member_key(2));
        submit_as(
            &mut host,
            &m1,
            1,
            gov_encode(&GovMsg::Propose {
                proposal_id: "mod-ghost".into(),
                action: GovAction::UpdateModule {
                    name: "replacement".into(),
                    module_id: "ghost".into(), // never registered
                    activation_lead: 500,
                    code_hash: hash(2),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect("propose");
        for (who, at) in [(&m1, 2u64), (&m2, 3u64)] {
            submit_as(
                &mut host,
                who,
                at,
                gov_encode(&GovMsg::Vote {
                    proposal_id: "mod-ghost".into(),
                    approve: true,
                }),
            )
            .await
            .expect("vote");
        }
        let err = submit_as(
            &mut host,
            &m2,
            4,
            gov_encode(&GovMsg::Execute {
                proposal_id: "mod-ghost".into(),
            }),
        )
        .await
        .expect_err("modreg refuses a swap for an unregistered module");
        assert!(matches!(err, SubmitError::Rejected(_)), "got {err:?}");
        assert_eq!(
            proposal_status(&host, "mod-ghost").await,
            Some(ProposalStatus::Open),
            "no partial state: the proposal remains Open"
        );
    });
}

/// door checks: a bad hash length and an unwired registry are refused at
/// Propose — the proposal is never created.
#[test]
fn door_checks_refuse_bad_hash_and_unwired_registry() {
    block_on(async {
        // bad hash length on a wired host.
        let mut host = gov_host_with_modreg().await;
        let m1 = member_key(1);
        let err = submit_as(
            &mut host,
            &m1,
            1,
            gov_encode(&GovMsg::Propose {
                proposal_id: "mod-short".into(),
                action: GovAction::UpdateModule {
                    name: "replacement".into(),
                    module_id: "hello".into(),
                    activation_lead: 500,
                    code_hash: vec![1, 2, 3],
                },
                voting_period: 100,
            }),
        )
        .await
        .expect_err("a 3-byte code_hash must be refused at the door");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("code_hash")),
            "got {err:?}"
        );
        assert_eq!(proposal_status(&host, "mod-short").await, None);

        // an UNWIRED registry (Governance::new without with_code_registry).
        let mut valset = Valset::new("valset", Box::new(MemStore::new()), "governance");
        valset.seed(member_key(1)).await.expect("seed valset");
        valset.finish_seed().await.expect("seed valset");
        let mut unwired = Host::genesis(vec![
            Box::new(valset),
            Box::new(Governance::new(
                "governance",
                Box::new(MemStore::new()),
                "valset",
                "identity",
            )),
        ])
        .expect("genesis");
        let err = submit_as(
            &mut unwired,
            &m1,
            1,
            gov_encode(&GovMsg::Propose {
                proposal_id: "mod-unwired".into(),
                action: GovAction::UpdateModule {
                    name: "replacement".into(),
                    module_id: "hello".into(),
                    activation_lead: 500,
                    code_hash: hash(2),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect_err("no code registry wired: refused at the door");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("registry")),
            "got {err:?}"
        );
    });
}

/// door check: an `activation_lead` at or under the registry's own floor
/// (`modules::MIN_SWAP_LEAD`) can never schedule successfully, whatever
/// height Execute lands at — refused at Propose, not discovered by every
/// Execute retrying and failing forever (#1775).
#[test]
fn door_check_refuses_a_lead_too_short_to_ever_schedule() {
    block_on(async {
        let mut host = gov_host_with_modreg().await;
        let m1 = member_key(1);
        let err = submit_as(
            &mut host,
            &m1,
            1,
            gov_encode(&GovMsg::Propose {
                proposal_id: "mod-short-lead".into(),
                action: GovAction::UpdateModule {
                    name: "replacement".into(),
                    module_id: "hello".into(),
                    activation_lead: modules::MIN_SWAP_LEAD,
                    code_hash: hash(2),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect_err("a lead at the registry's own floor can never schedule");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("activation_lead")),
            "got {err:?}"
        );
        assert_eq!(proposal_status(&host, "mod-short-lead").await, None);
    });
}

/// the module lookup, admission flavor: any id, absent allowed.
async fn module_code(host: &Host, id: &str) -> Option<modules::ModuleCode> {
    let reply = host
        .query("modules", &modules_query(&ModulesQuery::ModuleStatus))
        .await
        .expect("modreg status");
    match modules_decode(&reply).expect("decode") {
        ModulesReply::ModuleStatus { modules } => modules.into_iter().find(|m| m.module_id == id),
        other => panic!("expected Status, got {other:?}"),
    }
}

/// a passing RegisterModule ADMITS a brand-new module: the entry lands with an
/// EMPTY active hash and the pending initial code — registered, not yet
/// running, gated by the same readiness/height machinery as a swap.
#[test]
fn a_passing_register_module_admits_a_new_pending_entry() {
    block_on(async {
        let mut host = gov_host_with_modreg().await;
        assert!(module_code(&host, "kanban").await.is_none());
        pass(
            &mut host,
            1,
            "adm-1",
            GovAction::RegisterModule {
                name: "kanban-v1".into(),
                module_id: "kanban".into(),
                kind: modules::Kind::Module,
                activation_lead: 500,
                code_hash: hash(7),
                lanes: Vec::new(),
            },
        )
        .await;

        assert_eq!(
            proposal_status(&host, "adm-1").await,
            Some(ProposalStatus::Passed)
        );
        let code = module_code(&host, "kanban").await.expect("entry admitted");
        assert!(code.active_code_hash.is_empty(), "no active code until H");
        let pending = code.pending.expect("pending initial code landed");
        assert_eq!(pending.name, "kanban-v1");
        // same relative-lead arithmetic as the UpdateModule case above.
        assert_eq!(pending.activation_height, 504);
        assert_eq!(pending.code_hash, hash(7));
    });
}

/// a passing CancelModuleUpdate on an ADMISSION removes the entry entirely —
/// modreg never claims a module that has no code.
#[test]
fn a_passing_cancel_removes_an_admission_entry_entirely() {
    block_on(async {
        let mut host = gov_host_with_modreg().await;
        pass(
            &mut host,
            1,
            "adm-1",
            GovAction::RegisterModule {
                name: "kanban-v1".into(),
                module_id: "kanban".into(),
                kind: modules::Kind::Module,
                activation_lead: 500,
                code_hash: hash(7),
                lanes: Vec::new(),
            },
        )
        .await;
        assert!(module_code(&host, "kanban").await.is_some());

        pass(
            &mut host,
            10,
            "adm-cancel",
            GovAction::CancelModuleUpdate {
                name: "kanban-v1".into(),
                module_id: "kanban".into(),
            },
        )
        .await;
        assert!(
            module_code(&host, "kanban").await.is_none(),
            "cancelled admission leaves no registry entry"
        );
    });
}

/// a RegisterModule of an id that already exists is refused by the registry,
/// and the refusal fails the Execute op ATOMICALLY — the staged Passed
/// transition rolls back, the proposal stays Open, hello is untouched (the
/// exact contract `a_refused_schedule_fails_execute_atomically` pins for
/// swaps).
#[test]
fn register_module_of_an_existing_id_fails_execute_atomically() {
    block_on(async {
        let mut host = gov_host_with_modreg().await;
        let (m1, m2) = (member_key(1), member_key(2));
        submit_as(
            &mut host,
            &m1,
            1,
            gov_encode(&GovMsg::Propose {
                proposal_id: "adm-dup".into(),
                action: GovAction::RegisterModule {
                    name: "hello-again".into(),
                    module_id: "hello".into(),
                    kind: modules::Kind::Module,
                    activation_lead: 500,
                    code_hash: hash(9),
                    lanes: Vec::new(),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect("propose");
        for (who, at) in [(&m1, 2u64), (&m2, 3u64)] {
            submit_as(
                &mut host,
                who,
                at,
                gov_encode(&GovMsg::Vote {
                    proposal_id: "adm-dup".into(),
                    approve: true,
                }),
            )
            .await
            .expect("vote");
        }
        let refused = submit_as(
            &mut host,
            &m2,
            4,
            gov_encode(&GovMsg::Execute {
                proposal_id: "adm-dup".into(),
            }),
        )
        .await;
        assert!(refused.is_err(), "registry refusal fails the Execute op");
        assert_eq!(
            proposal_status(&host, "adm-dup").await,
            Some(ProposalStatus::Open),
            "staged Passed transition rolled back"
        );
        let code = module_code(&host, "hello").await.expect("hello persists");
        assert_eq!(code.active_code_hash, hash(1), "active code untouched");
        assert!(code.pending.is_none(), "no pending landed");
    });
}
