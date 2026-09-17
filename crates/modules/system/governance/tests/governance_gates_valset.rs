//! the governance loop end-to-end through a REAL host: a member proposes
//! admitting a validator, members vote, execution tallies deterministically
//! and performs the membership change by emitting the valset op as a
//! host-drained follow-up — while direct external valset writes are refused.
//!
//! ops are driven through `Host::submit_at` with `Origin::External(member
//! key)`, exactly the shape the ordered lane hands the host after VERIFYING a
//! frame signature — so what these tests pin is the authorization model the
//! live network runs.

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use futures::executor::block_on;
use governance::Governance;
use governance::{
    GovAction, GovMsg, GovQuery, GovReply, ProposalStatus, decode_reply as gov_decode,
    encode_msg as gov_encode, encode_query as gov_query,
};
use host::{BlockContext, Host, SubmitError};
use sdk::{Error, Msg, Origin};
use sdk_testkit::MemStore;
use valset::Valset;
use valset::{
    ValsetMsg, ValsetQuery, ValsetReply, decode_reply as valset_decode,
    encode_msg as valset_encode, encode_query as valset_query,
};

fn member_key(seed: u8) -> Vec<u8> {
    let seed = [seed; 32];
    PrivateKey::decode(&seed[..])
        .expect("any 32 bytes is a valid seed")
        .public_key()
        .as_ref()
        .to_vec()
}

/// a host with governance gating a valset seeded with members 1 and 2.
async fn gov_host() -> Host {
    let mut valset = Valset::new("valset", Box::new(MemStore::new()), "governance");
    valset.seed(member_key(1)).await.expect("seed valset");
    valset.seed(member_key(2)).await.expect("seed valset");
    valset.finish_seed().await.expect("seed valset");
    Host::genesis(vec![
        Box::new(valset),
        Box::new(Governance::new(
            "governance",
            Box::new(MemStore::new()),
            "valset",
            "identity",
        )),
    ])
    .expect("genesis")
}

/// submit one op as an EXTERNAL (verified-origin) submitter at a consensus time.
async fn submit_as(
    host: &mut Host,
    who: &[u8],
    at: u64,
    target: &str,
    payload: Vec<u8>,
) -> Result<(), SubmitError> {
    host.submit_at(
        BlockContext {
            height: at,
            consensus_time: at,
            origin: Origin::External(who.to_vec()),
        },
        Msg {
            target: target.into(),
            payload,
        },
    )
    .await
    .map(|_| ())
}

async fn validators(host: &Host) -> Vec<Vec<u8>> {
    let reply = host
        .query("valset", &valset_query(&ValsetQuery::Validators))
        .await
        .expect("valset query");
    match valset_decode(&reply).expect("decode") {
        ValsetReply::Validators(v) => v,
        other => panic!("expected Validators, got {other:?}"),
    }
}

async fn residents(host: &Host) -> Vec<Vec<u8>> {
    let reply = host
        .query("valset", &valset_query(&ValsetQuery::Residents))
        .await
        .expect("valset query");
    match valset_decode(&reply).expect("decode") {
        ValsetReply::Residents(v) => v,
        other => panic!("expected Residents, got {other:?}"),
    }
}

/// the roster-served listing's length — settled ids are evicted, so this
/// counts only currently-OPEN proposals.
async fn open_proposal_count(host: &Host) -> usize {
    let reply = host
        .query("governance", &gov_query(&GovQuery::Proposals))
        .await
        .expect("gov query");
    match gov_decode(&reply).expect("decode") {
        GovReply::Proposals(views) => views.len(),
        other => panic!("expected Proposals, got {other:?}"),
    }
}

/// grant `key` resident standing through the full ceremony, at heights
/// `at..at+2`. A validator is promoted OUT of the resident tier — valset's
/// header: "the tier a joiner syncs in before promotion, so the consensus set
/// only ever gains a caught-up validator" — so any test that admits a
/// validator starts here, the way a real joiner does.
async fn grant_standing(host: &mut Host, key: &[u8], voters: &[&[u8]], at: u64) {
    let id = format!("stand-{at}");
    let proposer = voters.first().expect("at least one voter");
    submit_as(
        host,
        proposer,
        at,
        "governance",
        gov_encode(&GovMsg::Propose {
            proposal_id: id.clone(),
            action: GovAction::AddResident { key: key.to_vec() },
            voting_period: 1_000_000,
        }),
    )
    .await
    .expect("propose AddResident");
    for who in voters {
        submit_as(
            host,
            who,
            at,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: id.clone(),
                approve: true,
            }),
        )
        .await
        .expect("vote AddResident");
    }
    submit_as(
        host,
        proposer,
        at,
        "governance",
        gov_encode(&GovMsg::Execute {
            proposal_id: id.clone(),
        }),
    )
    .await
    .expect("execute AddResident");
    assert!(
        residents(host).await.iter().any(|r| r == key),
        "standing granted before the promotion under test"
    );
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

#[test]
fn a_passing_proposal_admits_the_validator_and_direct_writes_are_refused() {
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2, newcomer) = (member_key(1), member_key(2), member_key(9));

        // DIRECT external valset writes are refused — the old one-message
        // liveness kill is closed.
        let err = submit_as(
            &mut host,
            &m1,
            1,
            "valset",
            valset_encode(&ValsetMsg::Leave { key: m2.clone() }),
        )
        .await
        .expect_err("external valset write must be refused");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("governance")),
            "got {err:?}"
        );
        assert_eq!(validators(&host).await.len(), 2, "membership untouched");

        grant_standing(&mut host, &newcomer, &[&m1, &m2], 1).await;

        // propose + both members vote yes -> early-decidable majority.
        submit_as(
            &mut host,
            &m1,
            2,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "add-9".into(),
                action: GovAction::AddValidator {
                    key: newcomer.clone(),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect("propose");
        submit_as(
            &mut host,
            &m1,
            3,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "add-9".into(),
                approve: true,
            }),
        )
        .await
        .expect("vote m1");
        submit_as(
            &mut host,
            &m2,
            4,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "add-9".into(),
                approve: true,
            }),
        )
        .await
        .expect("vote m2");

        // early execution: yes-ballots already form a strict majority. the
        // valset op rides the SAME block as a governance-origin follow-up.
        submit_as(
            &mut host,
            &m2,
            5,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "add-9".into(),
            }),
        )
        .await
        .expect("execute");

        assert_eq!(
            proposal_status(&host, "add-9").await,
            Some(ProposalStatus::Passed)
        );
        let members = validators(&host).await;
        assert_eq!(members.len(), 3, "the admitted validator is a member");
        assert!(members.contains(&newcomer));
    });
}

#[test]
fn an_add_resident_proposal_grants_resident_standing() {
    // end to end: a block proposes AddResident, a majority votes, and
    // execution emits the valset Grant follow-up — resident standing lands.
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2, friend) = (member_key(1), member_key(2), member_key(9));

        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "observe-9".into(),
                action: GovAction::AddResident {
                    key: friend.clone(),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect("propose AddResident at v0");
        for (who, at) in [(&m1, 2u64), (&m2, 3u64)] {
            submit_as(
                &mut host,
                who,
                at,
                "governance",
                gov_encode(&GovMsg::Vote {
                    proposal_id: "observe-9".into(),
                    approve: true,
                }),
            )
            .await
            .expect("vote");
        }
        submit_as(
            &mut host,
            &m2,
            4,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "observe-9".into(),
            }),
        )
        .await
        .expect("execute");

        assert_eq!(
            proposal_status(&host, "observe-9").await,
            Some(ProposalStatus::Passed)
        );
        assert_eq!(
            residents(&host).await,
            vec![friend],
            "resident standing granted from genesis"
        );
        assert_eq!(
            validators(&host).await.len(),
            2,
            "the validator set is untouched by a resident grant"
        );
    });
}

/// regression for #1776: an AddResident whose subject was promoted to
/// validator IN THE MEANTIME must settle cleanly instead of erroring the
/// whole Execute unit forever. valset's `handle_grant` Errs when the key is
/// already a validator ("resident standing is the pre-promotion tier") — an
/// unconditional emit would propagate that Err out of the follow-up and
/// reject Execute on every retry (the exact repro from the issue: an
/// AddResident and an AddValidator open for the same key at once, the
/// AddValidator executes first).
#[test]
fn an_add_resident_proposal_settles_cleanly_when_its_subject_is_already_a_validator() {
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2, key) = (member_key(1), member_key(2), member_key(9));

        // the key stands as a resident first — a promotion is only proposable
        // out of that tier — and `p1-resident` below is the SECOND mandate,
        // the one that goes stale when the promotion seats the key.
        grant_standing(&mut host, &key, &[&m1, &m2], 1).await;

        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "p1-resident".into(),
                action: GovAction::AddResident { key: key.clone() },
                voting_period: 100,
            }),
        )
        .await
        .expect("propose AddResident");
        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "p2-validator".into(),
                action: GovAction::AddValidator { key: key.clone() },
                voting_period: 100,
            }),
        )
        .await
        .expect("propose AddValidator");
        for id in ["p1-resident", "p2-validator"] {
            for (who, at) in [(&m1, 2u64), (&m2, 3u64)] {
                submit_as(
                    &mut host,
                    who,
                    at,
                    "governance",
                    gov_encode(&GovMsg::Vote {
                        proposal_id: id.into(),
                        approve: true,
                    }),
                )
                .await
                .expect("vote");
            }
        }

        // execute the validator admission FIRST: the key is seated.
        submit_as(
            &mut host,
            &m2,
            4,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "p2-validator".into(),
            }),
        )
        .await
        .expect("execute AddValidator");
        assert!(validators(&host).await.contains(&key));

        // now the stale AddResident mandate executes: valset would Err a
        // Grant for an already-seated validator — governance must pre-check
        // this and settle the proposal cleanly (Rejected) instead of
        // rejecting the whole Execute op.
        submit_as(
            &mut host,
            &m2,
            5,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "p1-resident".into(),
            }),
        )
        .await
        .expect("a stale AddResident settles cleanly, not with a rejected Execute op");

        assert_eq!(
            proposal_status(&host, "p1-resident").await,
            Some(ProposalStatus::Rejected),
            "a mandate that can never apply settles Rejected, not stuck Open forever"
        );
        assert!(
            !residents(&host).await.contains(&key),
            "the already-promoted key never lands as a resident too"
        );
    });
}

#[test]
fn a_passing_proposal_removes_the_validator_and_emits_leave() {
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2) = (member_key(1), member_key(2));
        assert_eq!(validators(&host).await.len(), 2, "seeded with two members");

        // the `member-remove` verb's on-chain shape: a member opens a
        // RemoveValidator proposal, members vote, and execution PERFORMS the
        // membership change by emitting `ValsetMsg::Leave` as a
        // governance-origin follow-up — the same path `node member promote` drives
        // for AddValidator/Join, inverted.
        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "remove-2".into(),
                action: GovAction::RemoveValidator { key: m2.clone() },
                voting_period: 100,
            }),
        )
        .await
        .expect("propose removal");
        // both current members vote yes -> early-decidable strict majority
        // (2 of 2). a member may cast the deciding ballot to remove another.
        submit_as(
            &mut host,
            &m1,
            2,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "remove-2".into(),
                approve: true,
            }),
        )
        .await
        .expect("vote m1");
        submit_as(
            &mut host,
            &m2,
            3,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "remove-2".into(),
                approve: true,
            }),
        )
        .await
        .expect("vote m2");

        // execution tallies and rides the valset Leave on the SAME block.
        submit_as(
            &mut host,
            &m2,
            4,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "remove-2".into(),
            }),
        )
        .await
        .expect("execute");

        assert_eq!(
            proposal_status(&host, "remove-2").await,
            Some(ProposalStatus::Passed)
        );
        let members = validators(&host).await;
        assert_eq!(members.len(), 1, "the removed validator is gone");
        assert!(members.contains(&m1), "the remaining member stays");
        assert!(
            !members.contains(&m2),
            "the removed key is dropped from the set"
        );
    });
}

/// the `member-leave` verb's on-chain shape: a member drives its OWN removal by
/// opening a RemoveValidator proposal for its own key and casting its yes-ballot
/// — the SAME governance path as `member-remove`, targeting self. this pins the
/// honesty of a unilateral leave: at n=2 the leaver's single ballot is NOT a
/// strict majority (majority = 2), so its own execute is not decidable early —
/// the removal stays PENDING until the remaining member also approves, and only
/// then does the leaver drop from the set.
#[test]
fn a_member_leaves_by_removing_itself_pending_the_remaining_majority() {
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2) = (member_key(1), member_key(2));
        assert_eq!(validators(&host).await.len(), 2, "seeded with two members");

        // m2 (the leaver) opens a self-removal and casts its own yes-ballot.
        submit_as(
            &mut host,
            &m2,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "leave-2".into(),
                action: GovAction::RemoveValidator { key: m2.clone() },
                voting_period: 100,
            }),
        )
        .await
        .expect("leaver proposes its own removal");
        submit_as(
            &mut host,
            &m2,
            2,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "leave-2".into(),
                approve: true,
            }),
        )
        .await
        .expect("leaver votes to leave");

        // the leaver's lone ballot is 1 of 2 — NOT a majority. its own execute
        // is refused as not-yet-decidable: leaving is not unilateral at n>=2.
        let err = submit_as(
            &mut host,
            &m2,
            3,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "leave-2".into(),
            }),
        )
        .await
        .expect_err("a lone leave ballot is not a deciding majority");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("not decidable")),
            "got {err:?}"
        );
        assert_eq!(
            validators(&host).await.len(),
            2,
            "the leaver is still in the set while pending"
        );

        // the remaining member approves -> strict majority (2 of 2) -> execute
        // removes the leaver at the valset Leave that rides the same block.
        submit_as(
            &mut host,
            &m1,
            4,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "leave-2".into(),
                approve: true,
            }),
        )
        .await
        .expect("remaining member approves the departure");
        submit_as(
            &mut host,
            &m1,
            5,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "leave-2".into(),
            }),
        )
        .await
        .expect("execute once the majority approves");

        assert_eq!(
            proposal_status(&host, "leave-2").await,
            Some(ProposalStatus::Passed)
        );
        let members = validators(&host).await;
        assert_eq!(members.len(), 1, "the leaver is gone");
        assert!(members.contains(&m1), "the remaining member stays");
        assert!(
            !members.contains(&m2),
            "the leaver's key is dropped from the set"
        );
    });
}

/// the solo-network tally: with one member, majority = 1/2 + 1 = 1, so the
/// founder's own ballot decides immediately — `node member promote` on a network
/// of one admits the friend in a single propose/vote/execute round, no
/// second party required.
#[test]
fn a_single_member_ballot_is_a_deciding_majority() {
    block_on(async {
        let founder = member_key(1);
        let friend = member_key(9);
        let mut valset = Valset::new("valset", Box::new(MemStore::new()), "governance");
        valset.seed(founder.clone()).await.expect("seed valset");
        valset.finish_seed().await.expect("seed valset");
        let mut host = Host::genesis(vec![
            Box::new(valset),
            Box::new(Governance::new(
                "governance",
                Box::new(MemStore::new()),
                "valset",
                "identity",
            )),
        ])
        .expect("genesis");

        grant_standing(&mut host, &friend, &[&founder], 1).await;

        submit_as(
            &mut host,
            &founder,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "add-friend".into(),
                action: GovAction::AddValidator {
                    key: friend.clone(),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect("propose");

        // proposing is NOT voting: executing with zero ballots is not
        // decidable before the deadline, even at n=1.
        let err = submit_as(
            &mut host,
            &founder,
            2,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "add-friend".into(),
            }),
        )
        .await
        .expect_err("no ballots -> not decidable early");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("not decidable"))
        );

        submit_as(
            &mut host,
            &founder,
            3,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "add-friend".into(),
                approve: true,
            }),
        )
        .await
        .expect("vote");
        submit_as(
            &mut host,
            &founder,
            4,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "add-friend".into(),
            }),
        )
        .await
        .expect("the single ballot decides");

        assert_eq!(
            proposal_status(&host, "add-friend").await,
            Some(ProposalStatus::Passed)
        );
        let members = validators(&host).await;
        assert_eq!(members.len(), 2, "the friend is admitted");
        assert!(members.contains(&friend));
    });
}

/// the last-validator guard at the governance layer: a solo member's ballot IS
/// a deciding majority (1 of 1), but enacting its own removal would empty the
/// validator set — a zero-validator orderer hits commonware `quorum(0)`, which
/// panics. governance refuses: it does NOT emit the set-emptying valset Leave,
/// marks the proposal Rejected instead, and the sole validator stays. this is
/// the "solo can't leave on-chain" case — a solo node forgets its workspace
/// locally rather than removing the last validator.
#[test]
fn removing_the_last_validator_is_refused_and_the_set_stays_non_empty() {
    block_on(async {
        let founder = member_key(1);
        let mut valset = Valset::new("valset", Box::new(MemStore::new()), "governance");
        valset.seed(founder.clone()).await.expect("seed valset");
        valset.finish_seed().await.expect("seed valset");
        let mut host = Host::genesis(vec![
            Box::new(valset),
            Box::new(Governance::new(
                "governance",
                Box::new(MemStore::new()),
                "valset",
                "identity",
            )),
        ])
        .expect("genesis");
        assert_eq!(validators(&host).await.len(), 1, "a solo network of one");

        // the sole member opens a self-removal and casts its own yes-ballot —
        // majority = 1/2 + 1 = 1, so this is early-decidable.
        submit_as(
            &mut host,
            &founder,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "leave-solo".into(),
                action: GovAction::RemoveValidator {
                    key: founder.clone(),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect("solo proposes its own removal");
        submit_as(
            &mut host,
            &founder,
            2,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "leave-solo".into(),
                approve: true,
            }),
        )
        .await
        .expect("solo votes to leave");

        // executing is a CLEAN op (the block commits): governance pre-checks that
        // the removal would empty the set and REJECTS the proposal rather than
        // emitting the set-emptying valset Leave. no block-abort, no panic.
        submit_as(
            &mut host,
            &founder,
            3,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "leave-solo".into(),
            }),
        )
        .await
        .expect("execute settles the proposal cleanly");

        assert_eq!(
            proposal_status(&host, "leave-solo").await,
            Some(ProposalStatus::Rejected),
            "a last-validator removal is refused, not passed"
        );
        let members = validators(&host).await;
        assert_eq!(members.len(), 1, "the set never went empty");
        assert!(members.contains(&founder), "the sole validator stays");
    });
}

/// belt-and-suspenders: even if a set-emptying valset `Leave` reached the valset
/// module directly (a module-origin write, bypassing governance's pre-check),
/// the valset handler itself refuses it. this pins the AUTHORITATIVE guard —
/// the invariant lives in the module that owns the set, not only in its caller.
#[test]
fn a_direct_module_origin_leave_of_the_last_validator_is_refused() {
    block_on(async {
        let founder = member_key(1);
        let mut valset = Valset::new("valset", Box::new(MemStore::new()), "governance");
        valset.seed(founder.clone()).await.expect("seed valset");
        valset.finish_seed().await.expect("seed valset");
        let mut host = Host::genesis(vec![Box::new(valset)]).expect("genesis");

        // a System-origin op (genesis orchestration shape) that would empty the
        // set is refused deterministically — the block is rejected, set intact.
        let err = host
            .submit_at(
                BlockContext {
                    height: 1,
                    consensus_time: 1,
                    origin: Origin::System,
                },
                Msg {
                    target: "valset".into(),
                    payload: valset_encode(&ValsetMsg::Leave {
                        key: founder.clone(),
                    }),
                },
            )
            .await
            .expect_err("emptying the set must be refused");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("last validator")),
            "got {err:?}"
        );
        assert_eq!(
            validators(&host).await,
            vec![founder],
            "the set is untouched"
        );
    });
}

#[test]
fn non_members_cannot_propose_or_vote_and_minority_rejects() {
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2, outsider) = (member_key(1), member_key(2), member_key(7));

        // an outsider cannot propose...
        let err = submit_as(
            &mut host,
            &outsider,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "p".into(),
                action: GovAction::Signal { text: "hi".into() },
                voting_period: 10,
            }),
        )
        .await
        .expect_err("outsider propose must be refused");
        assert!(matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("member")));

        // ...a member proposes, an outsider cannot vote...
        submit_as(
            &mut host,
            &m1,
            2,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "p".into(),
                action: GovAction::Signal { text: "hi".into() },
                voting_period: 10,
            }),
        )
        .await
        .expect("member propose");
        let err = submit_as(
            &mut host,
            &outsider,
            3,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
        )
        .await
        .expect_err("outsider vote must be refused");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("frozen electorate"))
        );

        // ...one yes of two members is NOT a strict majority: not decidable
        // early, and after the deadline it rejects.
        submit_as(
            &mut host,
            &m1,
            4,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
        )
        .await
        .expect("m1 votes yes");
        let err = submit_as(
            &mut host,
            &m2,
            5,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "p".into(),
            }),
        )
        .await
        .expect_err("not decidable before the deadline without a majority");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("not decidable"))
        );

        // past the deadline (proposed at 2, period 10 -> deadline 12).
        submit_as(
            &mut host,
            &m2,
            13,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "p".into(),
            }),
        )
        .await
        .expect("execute after deadline");
        assert_eq!(
            proposal_status(&host, "p").await,
            Some(ProposalStatus::Rejected)
        );
    });
}

#[test]
fn votes_close_at_the_deadline_and_ballots_are_per_member() {
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2) = (member_key(1), member_key(2));

        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "p".into(),
                action: GovAction::Signal { text: "x".into() },
                voting_period: 5,
            }),
        )
        .await
        .expect("propose");

        // a re-vote overwrites (yes -> no), it does not double-count.
        submit_as(
            &mut host,
            &m1,
            2,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
        )
        .await
        .expect("m1 yes");
        submit_as(
            &mut host,
            &m1,
            3,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: false,
            }),
        )
        .await
        .expect("m1 flips to no");

        // voting closes AT the deadline (proposed at 1, period 5 -> deadline 6).
        let err = submit_as(
            &mut host,
            &m2,
            6,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
        )
        .await
        .expect_err("vote at the deadline is closed");
        assert!(matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("closed")));

        // tally: zero CURRENT yes-ballots -> rejected.
        submit_as(
            &mut host,
            &m2,
            7,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "p".into(),
            }),
        )
        .await
        .expect("execute");
        assert_eq!(
            proposal_status(&host, "p").await,
            Some(ProposalStatus::Rejected)
        );
    });
}

#[test]
fn a_settled_proposal_frees_its_roster_slot() {
    // regression for the roster-filling DoS: once Execute tallies a
    // proposal (Passed or Rejected here), its id must leave the roster —
    // the listing narrows even though the RECORD (queried by id) survives.
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2) = (member_key(1), member_key(2));

        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "settle-me".into(),
                action: GovAction::Signal { text: "hi".into() },
                voting_period: 10,
            }),
        )
        .await
        .expect("propose");
        assert_eq!(open_proposal_count(&host).await, 1, "one open proposal");

        submit_as(
            &mut host,
            &m1,
            2,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "settle-me".into(),
                approve: true,
            }),
        )
        .await
        .expect("vote");
        submit_as(
            &mut host,
            &m2,
            3,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "settle-me".into(),
                approve: true,
            }),
        )
        .await
        .expect("vote");
        submit_as(
            &mut host,
            &m2,
            4,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "settle-me".into(),
            }),
        )
        .await
        .expect("execute");

        assert_eq!(
            proposal_status(&host, "settle-me").await,
            Some(ProposalStatus::Passed),
            "the record survives, queryable by id"
        );
        assert_eq!(
            open_proposal_count(&host).await,
            0,
            "the settled id left the open-proposal roster"
        );
    });
}

#[test]
fn a_settled_id_can_never_be_proposed_again() {
    // regression (#1766): a settled id leaves the roster but its RECORD is
    // kept forever, so a roster-only duplicate check let a second Propose
    // OVERWRITE the settled record with a fresh Open proposal. That erases a
    // decided outcome, and it is silent: a ceremony driver that waits for
    // "the proposal exists" is answered by the STALE record before its own
    // Propose lands, reports that old outcome, and votes on nothing.
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2) = (member_key(1), member_key(2));

        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "spent".into(),
                action: GovAction::Signal { text: "hi".into() },
                voting_period: 10,
            }),
        )
        .await
        .expect("propose");
        for (member, seq) in [(&m1, 2u64), (&m2, 3)] {
            submit_as(
                &mut host,
                member,
                seq,
                "governance",
                gov_encode(&GovMsg::Vote {
                    proposal_id: "spent".into(),
                    approve: true,
                }),
            )
            .await
            .expect("vote");
        }
        submit_as(
            &mut host,
            &m2,
            4,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "spent".into(),
            }),
        )
        .await
        .expect("execute");
        assert_eq!(
            proposal_status(&host, "spent").await,
            Some(ProposalStatus::Passed),
            "the id is settled Passed"
        );
        assert_eq!(
            open_proposal_count(&host).await,
            0,
            "and it left the open roster"
        );

        let reused = submit_as(
            &mut host,
            &m1,
            5,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "spent".into(),
                action: GovAction::Signal {
                    text: "again".into(),
                },
                voting_period: 10,
            }),
        )
        .await
        .expect_err("re-proposing a settled id must be refused");
        assert!(
            reused.to_string().contains("proposal already exists"),
            "unexpected refusal: {reused}"
        );
        assert_eq!(
            proposal_status(&host, "spent").await,
            Some(ProposalStatus::Passed),
            "the settled record is untouched by the refused reuse"
        );
    });
}

#[test]
fn an_expired_proposal_frees_its_roster_slot_on_the_next_propose() {
    // a proposal nobody ever executes still must not squat the roster past
    // its own voting deadline: `Propose` opportunistically reaps it.
    block_on(async {
        let mut host = gov_host().await;
        let m1 = member_key(1);

        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "stale".into(),
                action: GovAction::Signal {
                    text: "never executed".into(),
                },
                voting_period: 5,
            }),
        )
        .await
        .expect("propose");
        assert_eq!(open_proposal_count(&host).await, 1);
        assert_eq!(
            proposal_status(&host, "stale").await,
            Some(ProposalStatus::Open)
        );

        // past the deadline (proposed at 1, period 5 -> deadline 6) AND past
        // its execution grace; nobody ever calls Execute on "stale". a fresh
        // Propose well past deadline + EXECUTION_GRACE must reap it before
        // staging its own entry.
        submit_as(
            &mut host,
            &m1,
            200_000,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "fresh".into(),
                action: GovAction::Signal {
                    text: "reaps the stale one".into(),
                },
                voting_period: 10,
            }),
        )
        .await
        .expect("propose reaps the expired entry");

        assert_eq!(
            proposal_status(&host, "stale").await,
            Some(ProposalStatus::Rejected),
            "expiry settles it Rejected — nobody executed it in time"
        );
        assert_eq!(
            open_proposal_count(&host).await,
            1,
            "only the fresh proposal is open; the stale one was reaped"
        );
    });
}

#[test]
fn a_submitter_at_its_open_cap_is_refused_while_another_member_is_admitted() {
    // the per-submitter bound closes the roster-filling attack even before
    // any of the attacker's own proposals settle: MAX_OPEN_PROPOSALS_PER_SUBMITTER
    // (8) proposals from m1, long-lived so none can be reaped early, must
    // refuse a 9th from m1 while m2 can still propose.
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2) = (member_key(1), member_key(2));

        for n in 0..governance::MAX_OPEN_PROPOSALS_PER_SUBMITTER {
            submit_as(
                &mut host,
                &m1,
                1,
                "governance",
                gov_encode(&GovMsg::Propose {
                    proposal_id: format!("m1-{n}"),
                    action: GovAction::Signal {
                        text: "filler".into(),
                    },
                    voting_period: 1_000_000,
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("propose {n} from m1: {e:?}"));
        }
        assert_eq!(
            open_proposal_count(&host).await,
            governance::MAX_OPEN_PROPOSALS_PER_SUBMITTER
        );

        let err = submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "m1-over-cap".into(),
                action: GovAction::Signal {
                    text: "one too many".into(),
                },
                voting_period: 1_000_000,
            }),
        )
        .await
        .expect_err("m1 is already at its open-proposal cap");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("open proposals")),
            "got {err:?}"
        );

        // a DIFFERENT member is unaffected — the cap is per submitter, and
        // RemoveValidator (the eviction path) in particular must still go
        // through even while m1 has filled its own slots.
        submit_as(
            &mut host,
            &m2,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "evict-m1".into(),
                action: GovAction::RemoveValidator { key: m1.clone() },
                voting_period: 1_000_000,
            }),
        )
        .await
        .expect("m2 can still propose while m1 is at its cap");
        assert_eq!(
            open_proposal_count(&host).await,
            governance::MAX_OPEN_PROPOSALS_PER_SUBMITTER + 1
        );
    });
}

#[test]
fn a_passed_proposal_past_execution_grace_is_refused_and_reaped_rejected() {
    // a proposal that reached its yes-threshold but was NEVER executed must
    // not stay enactable forever: past deadline + EXECUTION_GRACE (100_000
    // consensus-time units) Execute settles it Rejected on the spot instead
    // of enacting a frozen, possibly long-gone electorate.
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2, newcomer) = (member_key(1), member_key(2), member_key(9));

        grant_standing(&mut host, &newcomer, &[&m1, &m2], 1).await;

        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "p".into(),
                action: GovAction::AddValidator {
                    key: newcomer.clone(),
                },
                voting_period: 5,
            }),
        )
        .await
        .expect("propose");

        // both members vote yes: required_yes = 2/2+1 = 2, met.
        submit_as(
            &mut host,
            &m1,
            2,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
        )
        .await
        .expect("m1 yes");
        submit_as(
            &mut host,
            &m2,
            3,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
        )
        .await
        .expect("m2 yes");

        // deadline is 1 + 5 = 6; nobody ever calls Execute. well past
        // deadline + EXECUTION_GRACE (100_000), Execute settles it Rejected
        // in place instead of enacting the frozen electorate's mandate — it
        // is an ordinary committed write (`Ok`), not an error, so the
        // settlement is never rolled back by an outer refusal.
        submit_as(
            &mut host,
            &m1,
            200_000,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "p".into(),
            }),
        )
        .await
        .expect("execute past the execution grace window settles it in place");
        assert_eq!(
            proposal_status(&host, "p").await,
            Some(ProposalStatus::Rejected),
            "expiry settles a passed-but-unexecuted proposal Rejected"
        );
        assert!(
            !validators(&host).await.contains(&newcomer),
            "an expired mandate must never be enacted"
        );
    });
}

#[test]
fn a_passed_proposal_inside_execution_grace_still_executes() {
    // symmetric to the above: past the plain deadline but still inside
    // EXECUTION_GRACE, Execute tallies and enacts normally.
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2, newcomer) = (member_key(1), member_key(2), member_key(9));

        grant_standing(&mut host, &newcomer, &[&m1, &m2], 1).await;

        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "p".into(),
                action: GovAction::AddValidator {
                    key: newcomer.clone(),
                },
                voting_period: 5,
            }),
        )
        .await
        .expect("propose");
        submit_as(
            &mut host,
            &m1,
            2,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
        )
        .await
        .expect("m1 yes");
        submit_as(
            &mut host,
            &m2,
            3,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            }),
        )
        .await
        .expect("m2 yes");

        // deadline is 6; far past it but well inside deadline + 100_000.
        submit_as(
            &mut host,
            &m1,
            50_000,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "p".into(),
            }),
        )
        .await
        .expect("still inside the execution grace window");
        assert_eq!(
            proposal_status(&host, "p").await,
            Some(ProposalStatus::Passed)
        );
        assert!(validators(&host).await.contains(&newcomer));
    });
}

#[test]
fn an_expired_proposal_is_reaped_by_a_later_vote_or_execute_on_another_proposal() {
    // expiry must not depend on someone else calling Propose: a later Vote
    // or Execute targeting a DIFFERENT, still-open proposal reaps the
    // long-expired one too.
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2) = (member_key(1), member_key(2));

        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "stale".into(),
                action: GovAction::Signal {
                    text: "never executed".into(),
                },
                voting_period: 5,
            }),
        )
        .await
        .expect("propose stale");

        // a second, long-lived proposal that is still open at the time we
        // act on it below.
        submit_as(
            &mut host,
            &m2,
            200_000,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "fresh".into(),
                action: GovAction::Signal {
                    text: "still open".into(),
                },
                voting_period: 1_000_000,
            }),
        )
        .await
        .expect("propose fresh (this ALSO reaps stale via Propose's own reap)");

        // "stale"'s deadline was 6; "fresh" was proposed at 200_000, which
        // is already past stale's deadline + EXECUTION_GRACE (100_006), so
        // Propose's own reap already caught it. Cast a Vote on "fresh" at a
        // LATER time to prove Vote's reap is independently wired too (using
        // a second stale proposal created after the first reap already ran).
        submit_as(
            &mut host,
            &m1,
            200_001,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "stale-2".into(),
                action: GovAction::Signal {
                    text: "never executed either".into(),
                },
                voting_period: 5,
            }),
        )
        .await
        .expect("propose stale-2");
        assert_eq!(
            proposal_status(&host, "stale-2").await,
            Some(ProposalStatus::Open)
        );

        submit_as(
            &mut host,
            &m2,
            400_000,
            "governance",
            gov_encode(&GovMsg::Vote {
                proposal_id: "fresh".into(),
                approve: true,
            }),
        )
        .await
        .expect("vote on fresh reaps stale-2 too");

        assert_eq!(
            proposal_status(&host, "stale-2").await,
            Some(ProposalStatus::Rejected),
            "Vote's own reap_roster call caught the unrelated expired proposal"
        );
    });
}

/// #2507: `ducktape node member promote <any 64-hex>` seated a key nobody had
/// ever met. The CLI checked only "is it already a validator", and valset is
/// deliberately permissionless ("no authorization, no gating", its own
/// header), so nothing between the terminal and the quorum asked whether a
/// node stood behind the key. The next epoch cutover grew the quorum to
/// 2-of-2 with one seat that would never vote, and the chain stopped. The undo
/// is itself a proposal, so it then needed the phantom's ballot: the network
/// was gone.
///
/// Governance is the layer that decides who may be seated, so the refusal
/// lands at the door, where the proposer is still reading the answer.
#[test]
fn a_promotion_of_a_key_that_holds_no_resident_standing_is_refused_at_the_door() {
    block_on(async {
        let mut host = gov_host().await;
        let (m1, stranger) = (member_key(1), member_key(9));

        let refusal = submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "promote-a-stranger".into(),
                action: GovAction::AddValidator {
                    key: stranger.clone(),
                },
                voting_period: 100,
            }),
        )
        .await
        .expect_err("a key with no resident record cannot be proposed for a seat");
        assert!(
            format!("{refusal:?}").contains("not_a_resident"),
            "the refusal carries its token: {refusal:?}"
        );

        assert_eq!(
            proposal_status(&host, "promote-a-stranger").await,
            None,
            "nothing was opened, so there is no ballot to carry"
        );
        assert_eq!(validators(&host).await.len(), 2, "the quorum is untouched");
    });
}

/// The door is not the last word. Standing can be revoked while the ballot is
/// open, and a proposal can arrive from a peer's synced store minted before
/// this rule — either way the Execute must not seat it. Settle `Rejected`, the
/// way the capacity and set-emptying cases beside it already do.
#[test]
fn a_promotion_whose_subject_loses_its_standing_before_execute_settles_rejected() {
    block_on(async {
        let mut host = gov_host().await;
        let (m1, m2, friend) = (member_key(1), member_key(2), member_key(9));

        // grant standing, so the promotion is legal to propose at all.
        submit_as(
            &mut host,
            &m1,
            1,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "stand-9".into(),
                action: GovAction::AddResident {
                    key: friend.clone(),
                },
                voting_period: 1000,
            }),
        )
        .await
        .expect("propose AddResident");
        for (who, at) in [(&m1, 2u64), (&m2, 3u64)] {
            submit_as(
                &mut host,
                who,
                at,
                "governance",
                gov_encode(&GovMsg::Vote {
                    proposal_id: "stand-9".into(),
                    approve: true,
                }),
            )
            .await
            .expect("vote");
        }
        submit_as(
            &mut host,
            &m2,
            4,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "stand-9".into(),
            }),
        )
        .await
        .expect("execute AddResident");
        assert_eq!(residents(&host).await, vec![friend.clone()]);

        // open the promotion and carry it to executable, but do not execute.
        submit_as(
            &mut host,
            &m1,
            10,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "promote-9".into(),
                action: GovAction::AddValidator {
                    key: friend.clone(),
                },
                voting_period: 1000,
            }),
        )
        .await
        .expect("a resident may be proposed for a seat");
        for (who, at) in [(&m1, 11u64), (&m2, 12u64)] {
            submit_as(
                &mut host,
                who,
                at,
                "governance",
                gov_encode(&GovMsg::Vote {
                    proposal_id: "promote-9".into(),
                    approve: true,
                }),
            )
            .await
            .expect("vote");
        }

        // standing is revoked underneath the open ballot.
        submit_as(
            &mut host,
            &m1,
            20,
            "governance",
            gov_encode(&GovMsg::Propose {
                proposal_id: "unstand-9".into(),
                action: GovAction::RemoveResident {
                    key: friend.clone(),
                },
                voting_period: 1000,
            }),
        )
        .await
        .expect("propose RemoveResident");
        for (who, at) in [(&m1, 21u64), (&m2, 22u64)] {
            submit_as(
                &mut host,
                who,
                at,
                "governance",
                gov_encode(&GovMsg::Vote {
                    proposal_id: "unstand-9".into(),
                    approve: true,
                }),
            )
            .await
            .expect("vote");
        }
        submit_as(
            &mut host,
            &m2,
            23,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "unstand-9".into(),
            }),
        )
        .await
        .expect("execute RemoveResident");
        assert!(
            residents(&host).await.is_empty(),
            "standing is gone before the promotion executes"
        );

        submit_as(
            &mut host,
            &m2,
            30,
            "governance",
            gov_encode(&GovMsg::Execute {
                proposal_id: "promote-9".into(),
            }),
        )
        .await
        .expect("a mandate that can no longer apply settles, it does not error the unit");

        assert_eq!(
            proposal_status(&host, "promote-9").await,
            Some(ProposalStatus::Rejected),
            "the seat is refused rather than granted to a key that stands for nothing"
        );
        assert!(
            !validators(&host).await.contains(&friend),
            "the phantom never reaches the quorum"
        );
        assert_eq!(validators(&host).await.len(), 2, "the quorum is unchanged");
    });
}
