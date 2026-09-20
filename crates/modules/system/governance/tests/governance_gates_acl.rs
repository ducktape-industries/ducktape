//! regression for #1777: a passing `SetAclPolicy` must never lock the
//! electorate that decides governance's own future proposals out of
//! governance itself — `SetPolicy` is reachable only through a governance
//! follow-up, so a policy neither ballot kind can satisfy would brick the
//! module (and everything gated behind it) permanently, with no repair
//! proposal able to reach the door that just closed on it.
//!
//! ops are driven through a REAL `Host` with a REAL `acl::Acl` module wired,
//! exactly the composition a live network runs.

use acl_module::{
    Acl, AclQuery, AclReply, Standing as AclStanding, decode_reply as acl_decode,
    encode_query as acl_query,
};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use futures::executor::block_on;
use governance::Governance;
use governance::{
    GovAction, GovMsg, GovQuery, GovReply, ProposalStatus, Standing, decode_reply as gov_decode,
    encode_msg as gov_encode, encode_query as gov_query,
};
use host::{BlockContext, Host, SubmitError};
use sdk::{Error, Msg, Origin};
use sdk_testkit::MemStore;
use valset_module::Valset;

fn member_key(seed: u8) -> Vec<u8> {
    let seed = [seed; 32];
    PrivateKey::decode(&seed[..])
        .expect("any 32 bytes is a valid seed")
        .public_key()
        .as_ref()
        .to_vec()
}

/// a host with valset (members 1,2), governance wired to a REAL acl module.
async fn gov_host_with_acl() -> Host {
    let mut valset = Valset::new("valset", Box::new(MemStore::new()), "governance");
    valset.seed(member_key(1)).await.expect("seed valset");
    valset.seed(member_key(2)).await.expect("seed valset");
    valset.finish_seed().await.expect("seed valset");
    Host::genesis(vec![
        Box::new(valset),
        Box::new(
            Governance::new(
                "governance",
                Box::new(MemStore::new()),
                "valset",
                "identity",
            )
            .with_acl("acl"),
        ),
        Box::new(Acl::new("acl", Box::new(MemStore::new()), "governance")),
    ])
    .expect("genesis")
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

async fn policy_for(host: &Host, target: &str) -> Option<AclStanding> {
    let reply = host
        .query(
            "acl",
            &acl_query(&AclQuery::PolicyFor {
                target: target.into(),
            }),
        )
        .await
        .expect("acl query");
    match acl_decode(&reply).expect("decode") {
        AclReply::PolicyFor(standing) => standing,
        other => panic!("expected PolicyFor, got {other:?}"),
    }
}

/// propose an action at `base`, both members vote yes, Execute — the early
/// strict-majority path settles in the same round.
async fn run_to_execute(
    host: &mut Host,
    base: u64,
    id: &str,
    action: GovAction,
) -> Result<(), SubmitError> {
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
    .await?;
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
        .await?;
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
}

/// the headline repro: in the DEFAULT validator-ballot mode, a policy
/// requiring `User` standing on `governance` is refused at the DOOR — a node
/// key is never an Identity account, so no validator could ever submit a
/// future Propose/Vote/Execute/Redeem again.
#[test]
fn a_user_standing_policy_on_governance_is_refused_at_propose_in_validator_mode() {
    block_on(async {
        let mut host = gov_host_with_acl().await;
        let m1 = member_key(1);
        let err = submit_as(
            &mut host,
            &m1,
            1,
            gov_encode(&GovMsg::Propose {
                proposal_id: "brick-it".into(),
                action: GovAction::SetAclPolicy {
                    target: "governance".into(),
                    standing: Some(Standing::User),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect_err("a policy the validator-mode electorate can never satisfy is refused");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module { ref reason, .. }) if reason == "electorate_lockout"),
            "got {err:?}"
        );
        assert_eq!(proposal_status(&host, "brick-it").await, None);
        assert_eq!(policy_for(&host, "governance").await, None, "untouched");
    });
}

/// the same lockout via the `"*"` wildcard fallback — tightening everything
/// closes governance's own door exactly as directly naming it would.
#[test]
fn a_user_standing_wildcard_policy_is_also_refused_at_propose() {
    block_on(async {
        let mut host = gov_host_with_acl().await;
        let m1 = member_key(1);
        let err = submit_as(
            &mut host,
            &m1,
            1,
            gov_encode(&GovMsg::Propose {
                proposal_id: "brick-all".into(),
                action: GovAction::SetAclPolicy {
                    target: "*".into(),
                    standing: Some(Standing::User),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect_err("a wildcard policy that would brick governance is refused too");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module { ref reason, .. }) if reason == "electorate_lockout"),
            "got {err:?}"
        );
    });
}

/// the fix does not over-block: a policy the validator-mode electorate CAN
/// still satisfy (`Node`, its own standing) proposes and executes normally,
/// and every OTHER target is unaffected by the self-lockout rule entirely.
#[test]
fn a_satisfiable_policy_on_governance_and_any_policy_elsewhere_still_pass() {
    block_on(async {
        let mut host = gov_host_with_acl().await;
        run_to_execute(
            &mut host,
            1,
            "tighten-governance",
            GovAction::SetAclPolicy {
                target: "governance".into(),
                standing: Some(Standing::Node),
            },
        )
        .await
        .expect("Node standing is what the validator electorate already holds");
        assert_eq!(
            proposal_status(&host, "tighten-governance").await,
            Some(ProposalStatus::Passed)
        );
                assert_eq!(policy_for(&host, "governance").await, Some(AclStanding::Node));

        run_to_execute(
            &mut host,
            10,
            "tighten-chat",
            GovAction::SetAclPolicy {
                target: "chat".into(),
                standing: Some(Standing::User),
            },
        )
        .await
        .expect("a policy on an unrelated target is never gated by this rule");
        assert_eq!(policy_for(&host, "chat").await, Some(AclStanding::User));
    });
}
