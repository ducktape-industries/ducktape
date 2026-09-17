//! Non-transferable account shares through a real Host. The validator-mode
//! electorate adopts one explicit allocation; later proposals freeze account
//! power, so two keys of one account cast ONE account ballot and later share
//! changes cannot rewrite an open proposal's decision boundary.

use std::collections::BTreeMap;

use commonware_codec::DecodeExt as _;
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use futures::executor::block_on;
use governance::{
    GovAction, GovMsg, GovQuery, GovReply, Governance, ProposalStatus, ShareAllocation, VoterKind,
    VotingRule, decode_reply, encode_msg, encode_query,
};
use host::{BlockContext, Host, SubmitError};
use identity::{
    AccountView, IdentityQuery, IdentityReply, KeyScheme, KeyView, account_principal,
    decode_query as identity_decode_query, encode_reply as identity_encode_reply,
};
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot, StateSyncHandle};
use sdk_testkit::MemStore;
use valset_module::Valset;

fn key(seed: u8) -> Vec<u8> {
    let seed = [seed; 32];
    PrivateKey::decode(&seed[..])
        .expect("valid seed")
        .public_key()
        .as_ref()
        .to_vec()
}

/// a read-only identity: accounts by number, each owning its member keys —
/// the real module's `OfKey` index, without the admission ceremony.
struct IdentityStub {
    accounts: BTreeMap<u64, AccountView>,
    by_key: BTreeMap<Vec<u8>, u64>,
}

impl IdentityStub {
    /// `entries`: account number → its member keys.
    fn new(entries: Vec<(u64, Vec<Vec<u8>>)>) -> Self {
        let mut accounts = BTreeMap::new();
        let mut by_key = BTreeMap::new();
        for (number, mut keys) in entries {
            keys.sort();
            for key in &keys {
                by_key.insert(key.clone(), number);
            }
            accounts.insert(
                number,
                AccountView {
                    control: identity::Control::Keys,
                    number,
                    name: format!("account-{number}"),
                    keys: keys
                        .into_iter()
                        .map(|pubkey| KeyView {
                            scheme: KeyScheme::Ed25519,
                            pubkey,
                            label: None,
                            added_at: 0,
                        })
                        .collect(),
                    avatar: None,
                    bio: None,
                    updated_at: 0,
                },
            );
        }
        Self { accounts, by_key }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for IdentityStub {
    fn id(&self) -> ModuleId {
        "identity".into()
    }

    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::Stateless)
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        Err(Error::Module("identity test stub is read-only".into()))
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let reply = match identity_decode_query(req).map_err(Error::Module)? {
            IdentityQuery::All { from, limit } => IdentityReply::Accounts(
                self.accounts
                    .range(from..)
                    .take(limit as usize)
                    .map(|(_, account)| account.clone())
                    .collect(),
            ),
            IdentityQuery::Get { number } => {
                IdentityReply::Account(self.accounts.get(&number).cloned())
            }
            IdentityQuery::OfKey { key } => IdentityReply::Account(
                self.by_key
                    .get(&key)
                    .and_then(|number| self.accounts.get(number))
                    .cloned(),
            ),
            IdentityQuery::KeyGen { key } => {
                IdentityReply::Gen(u64::from(self.by_key.contains_key(&key)))
            }
            IdentityQuery::Controlled { .. } => IdentityReply::Accounts(Vec::new()),
            IdentityQuery::Resolve { .. } => {
                return Err(Error::Module("unexpected bulk identity query".into()));
            }
        };
        Ok(identity_encode_reply(&reply))
    }
}

/// validators = keys[0..3]; account 1 holds keys[0] and keys[1] ("one human,
/// two devices"), account 2 holds keys[2], account 3 holds keys[3] (not a
/// validator).
async fn share_host() -> (Host, [Vec<u8>; 4], [u64; 3]) {
    let keys = [key(1), key(2), key(3), key(4)];
    let accounts = [1, 2, 3];
    let mut valset = Valset::new("valset", Box::new(MemStore::new()), "governance");
    for node in &keys[..3] {
        valset.seed(node.clone()).await.expect("seed valset");
    }
    valset.finish_seed().await.expect("seed valset");
    let identity = IdentityStub::new(vec![
        (accounts[0], vec![keys[0].clone(), keys[1].clone()]),
        (accounts[1], vec![keys[2].clone()]),
        (accounts[2], vec![keys[3].clone()]),
    ]);
    let host = Host::genesis(vec![
        Box::new(valset),
        Box::new(Governance::new(
            "governance",
            Box::new(MemStore::new()),
            "valset",
            "identity",
        )),
        Box::new(identity),
    ])
    .expect("genesis");
    (host, keys, accounts)
}

async fn submit(host: &mut Host, node: &[u8], at: u64, msg: GovMsg) -> Result<(), SubmitError> {
    host.submit_at(
        BlockContext {
            height: at,
            consensus_time: at,
            origin: Origin::External(node.to_vec()),
        },
        Msg {
            target: "governance".into(),
            payload: encode_msg(&msg),
        },
    )
    .await
    .map(|_| ())
}

async fn proposal(host: &Host, id: &str) -> governance::ProposalView {
    let reply = host
        .query(
            "governance",
            &encode_query(&GovQuery::Proposal {
                proposal_id: id.into(),
            }),
        )
        .await
        .expect("proposal query");
    let GovReply::Proposal(Some(proposal)) = decode_reply(&reply).expect("decode") else {
        panic!("proposal exists")
    };
    proposal
}

async fn shares(host: &Host) -> governance::SharesView {
    let reply = host
        .query("governance", &encode_query(&GovQuery::Shares))
        .await
        .expect("shares query");
    let GovReply::Shares(shares) = decode_reply(&reply).expect("decode") else {
        panic!("shares reply")
    };
    shares
}

#[test]
fn shares_are_account_scoped_weighted_and_frozen_per_proposal() {
    block_on(async {
        let (mut host, keys, accounts) = share_host().await;
        let defaults = shares(&host).await;
        assert!(!defaults.active, "validator ballots are the default mode");
        assert!(defaults.allocations.is_empty());
        submit(
            &mut host,
            &keys[0],
            0,
            GovMsg::Propose {
                proposal_id: "premature-share-mode".into(),
                action: GovAction::SetShareMode { enabled: true },
                voting_period: 20,
            },
        )
        .await
        .expect_err("share mode requires a configured registry");
        let initial = vec![
            ShareAllocation {
                account_id: accounts[2],
                shares: 10,
            },
            ShareAllocation {
                account_id: accounts[0],
                shares: 60,
            },
            ShareAllocation {
                account_id: accounts[1],
                shares: 30,
            },
        ];

        submit(
            &mut host,
            &keys[0],
            1,
            GovMsg::Propose {
                proposal_id: "adopt".into(),
                action: GovAction::AdoptShares {
                    allocations: initial,
                },
                voting_period: 20,
            },
        )
        .await
        .expect("propose adoption");
        for (node, at) in [(&keys[0], 2), (&keys[1], 3)] {
            submit(
                &mut host,
                node,
                at,
                GovMsg::Vote {
                    proposal_id: "adopt".into(),
                    approve: true,
                },
            )
            .await
            .expect("validator-mode ballot on the adopt proposal");
        }
        let adoption = proposal(&host, "adopt").await;
        assert_eq!(adoption.voter_kind, VoterKind::ValidatorNode);
        assert_eq!(
            adoption.votes.len(),
            2,
            "two validator keys of one account are two node ballots in validator mode"
        );
        submit(
            &mut host,
            &keys[2],
            4,
            GovMsg::Execute {
                proposal_id: "adopt".into(),
            },
        )
        .await
        .expect("adopt shares");

        let adopted = shares(&host).await;
        assert!(adopted.active);
        assert_eq!(adopted.total, 100);
        assert_eq!(
            adopted
                .allocations
                .iter()
                .map(|allocation| allocation.account_id)
                .collect::<Vec<_>>(),
            accounts.to_vec(),
            "the adoption action is normalized by account number"
        );

        // a key that belongs to no account has no share-mode standing at all.
        let err = submit(
            &mut host,
            &key(200),
            4,
            GovMsg::Propose {
                proposal_id: "stranger".into(),
                action: GovAction::Signal {
                    text: "nope".into(),
                },
                voting_period: 20,
            },
        )
        .await
        .expect_err("a key of no account cannot propose in share mode");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("no Identity account")),
            "rejection names the missing account, got {err:?}"
        );

        // Open a signal before changing C's shares. Its 60/30/10 electorate
        // must remain unchanged after the structural update lands.
        submit(
            &mut host,
            &keys[0],
            5,
            GovMsg::Propose {
                proposal_id: "signal".into(),
                action: GovAction::Signal {
                    text: "ship".into(),
                },
                voting_period: 20,
            },
        )
        .await
        .expect("shareholder signal");
        submit(
            &mut host,
            &keys[0],
            6,
            GovMsg::Propose {
                proposal_id: "raise-c".into(),
                action: GovAction::SetShares {
                    account_id: accounts[2],
                    shares: 20,
                },
                voting_period: 20,
            },
        )
        .await
        .expect("structural proposal");

        // account 1 has two keys but only one 60-share ballot. Its second key
        // overwrites the same principal and cannot reach the 67-share
        // structural threshold by itself.
        for (node, at) in [(&keys[0], 7), (&keys[1], 8)] {
            submit(
                &mut host,
                node,
                at,
                GovMsg::Vote {
                    proposal_id: "raise-c".into(),
                    approve: true,
                },
            )
            .await
            .expect("account A vote");
        }
        let structural = proposal(&host, "raise-c").await;
        assert_eq!(structural.voter_kind, VoterKind::Account);
        assert_eq!(
            structural.votes,
            vec![(account_principal(accounts[0]), true)],
            "two keys of one account cast ONE account ballot"
        );
        assert_eq!(
            structural.voting_rule,
            VotingRule::Threshold { required_yes: 67 }
        );
        submit(
            &mut host,
            &keys[0],
            9,
            GovMsg::Execute {
                proposal_id: "raise-c".into(),
            },
        )
        .await
        .expect_err("60 shares cannot decide a two-thirds action");

        submit(
            &mut host,
            &keys[2],
            10,
            GovMsg::Vote {
                proposal_id: "raise-c".into(),
                approve: true,
            },
        )
        .await
        .expect("account B vote");
        submit(
            &mut host,
            &keys[2],
            11,
            GovMsg::Execute {
                proposal_id: "raise-c".into(),
            },
        )
        .await
        .expect("90 shares pass the structural action");
        assert_eq!(shares(&host).await.total, 110);

        let signal = proposal(&host, "signal").await;
        assert_eq!(signal.voter_kind, VoterKind::Account);
        assert_eq!(
            signal.voting_rule,
            VotingRule::ParticipatingMajority { quorum: 50 }
        );
        assert_eq!(
            signal
                .electorate
                .iter()
                .find(|(account, _)| account == &account_principal(accounts[2]))
                .map(|(_, power)| *power),
            Some(10),
            "an open proposal keeps its proposal-time share snapshot"
        );

        // account 1's second key flips its ballot rather than adding another.
        submit(
            &mut host,
            &keys[0],
            12,
            GovMsg::Vote {
                proposal_id: "signal".into(),
                approve: true,
            },
        )
        .await
        .expect("A yes");
        submit(
            &mut host,
            &keys[1],
            13,
            GovMsg::Vote {
                proposal_id: "signal".into(),
                approve: false,
            },
        )
        .await
        .expect("account 1 flips through its other key");
        submit(
            &mut host,
            &keys[2],
            14,
            GovMsg::Vote {
                proposal_id: "signal".into(),
                approve: true,
            },
        )
        .await
        .expect("B yes");
        submit(
            &mut host,
            &keys[3],
            15,
            GovMsg::Vote {
                proposal_id: "signal".into(),
                approve: true,
            },
        )
        .await
        .expect("C yes");
        assert_eq!(proposal(&host, "signal").await.votes.len(), 3);
        submit(
            &mut host,
            &keys[0],
            26,
            GovMsg::Execute {
                proposal_id: "signal".into(),
            },
        )
        .await
        .expect("settle after deadline");
        assert_eq!(
            proposal(&host, "signal").await.status,
            ProposalStatus::Rejected
        );

        // The current share electorate can restore the default validator mode.
        submit(
            &mut host,
            &keys[0],
            27,
            GovMsg::Propose {
                proposal_id: "validator-mode".into(),
                action: GovAction::SetShareMode { enabled: false },
                voting_period: 20,
            },
        )
        .await
        .expect("propose validator mode");
        for (node, at) in [(&keys[0], 28), (&keys[2], 29)] {
            submit(
                &mut host,
                node,
                at,
                GovMsg::Vote {
                    proposal_id: "validator-mode".into(),
                    approve: true,
                },
            )
            .await
            .expect("shareholder approves validator mode");
        }
        submit(
            &mut host,
            &keys[2],
            30,
            GovMsg::Execute {
                proposal_id: "validator-mode".into(),
            },
        )
        .await
        .expect("switch to validator mode");
        let inactive = shares(&host).await;
        assert!(!inactive.active);
        assert_eq!(inactive.total, 110, "switching modes retains allocations");

        // In validator mode, account 1's two validator keys again count
        // separately.
        submit(
            &mut host,
            &keys[0],
            31,
            GovMsg::Propose {
                proposal_id: "validator-signal".into(),
                action: GovAction::Signal {
                    text: "one validator, one ballot".into(),
                },
                voting_period: 20,
            },
        )
        .await
        .expect("validator-mode signal");
        let validator_signal = proposal(&host, "validator-signal").await;
        assert_eq!(validator_signal.voter_kind, VoterKind::ValidatorNode);
        assert_eq!(validator_signal.electorate.len(), 3);
        assert!(
            validator_signal
                .electorate
                .iter()
                .all(|(_, power)| *power == 1)
        );
        assert_eq!(
            validator_signal.voting_rule,
            VotingRule::Threshold { required_yes: 2 }
        );
        for (node, at) in [(&keys[0], 32), (&keys[1], 33)] {
            submit(
                &mut host,
                node,
                at,
                GovMsg::Vote {
                    proposal_id: "validator-signal".into(),
                    approve: true,
                },
            )
            .await
            .expect("validator ballot");
        }
        assert_eq!(proposal(&host, "validator-signal").await.votes.len(), 2);
        submit(
            &mut host,
            &keys[2],
            34,
            GovMsg::Execute {
                proposal_id: "validator-signal".into(),
            },
        )
        .await
        .expect("validator majority passes");

        // The validator electorate can enable the retained account registry.
        submit(
            &mut host,
            &keys[0],
            35,
            GovMsg::Propose {
                proposal_id: "share-mode".into(),
                action: GovAction::SetShareMode { enabled: true },
                voting_period: 20,
            },
        )
        .await
        .expect("propose share mode");
        for (node, at) in [(&keys[0], 36), (&keys[1], 37)] {
            submit(
                &mut host,
                node,
                at,
                GovMsg::Vote {
                    proposal_id: "share-mode".into(),
                    approve: true,
                },
            )
            .await
            .expect("validator approves share mode");
        }
        submit(
            &mut host,
            &keys[2],
            38,
            GovMsg::Execute {
                proposal_id: "share-mode".into(),
            },
        )
        .await
        .expect("switch back to account shares");
        assert!(shares(&host).await.active);
    });
}

// ── validator mode seats node keys, never accounts ──
//
// a node key is never an account: in validator mode the submitter must ITSELF
// be a member node, its ballot is its own, and belonging to an account changes
// nothing — no fan-out to the account's other keys, no account principal.

#[test]
fn validator_mode_keys_a_ballot_by_node_even_for_an_account_key() {
    block_on(async {
        // keys[0] and keys[1] are both validators AND both keys of account 1.
        let (mut host, keys, accounts) = share_host().await;
        submit(
            &mut host,
            &keys[0],
            1,
            GovMsg::Propose {
                proposal_id: "node-signal".into(),
                action: GovAction::Signal { text: "hi".into() },
                voting_period: 50,
            },
        )
        .await
        .expect("a member node proposes");

        let opened = proposal(&host, "node-signal").await;
        assert_eq!(opened.voter_kind, VoterKind::ValidatorNode);
        assert_eq!(
            opened.proposer, keys[0],
            "the proposer is the node key, not account 1's principal"
        );
        let mut electorate: Vec<Vec<u8>> =
            opened.electorate.iter().map(|(k, _)| k.clone()).collect();
        electorate.sort();
        let mut want = vec![keys[0].clone(), keys[1].clone(), keys[2].clone()];
        want.sort();
        assert_eq!(
            electorate, want,
            "the electorate is the three validator nodes"
        );
        assert!(
            !opened
                .electorate
                .iter()
                .any(|(k, _)| k == &account_principal(accounts[0])),
            "no account principal seats a validator-mode electorate"
        );

        // keys[0]'s ballot is its own — keys[1] gains nothing from sharing
        // the account.
        submit(
            &mut host,
            &keys[0],
            2,
            GovMsg::Vote {
                proposal_id: "node-signal".into(),
                approve: true,
            },
        )
        .await
        .expect("node vote");
        assert_eq!(
            proposal(&host, "node-signal").await.votes,
            vec![(keys[0].clone(), true)],
            "one node key, one ballot"
        );
        submit(
            &mut host,
            &keys[1],
            3,
            GovMsg::Vote {
                proposal_id: "node-signal".into(),
                approve: true,
            },
        )
        .await
        .expect("second node vote");
        assert_eq!(proposal(&host, "node-signal").await.votes.len(), 2);
        submit(
            &mut host,
            &keys[2],
            4,
            GovMsg::Execute {
                proposal_id: "node-signal".into(),
            },
        )
        .await
        .expect("2 of 3 passes");
        assert_eq!(
            proposal(&host, "node-signal").await.status,
            ProposalStatus::Passed
        );
    });
}

#[test]
fn a_non_validator_key_is_refused_in_validator_mode() {
    block_on(async {
        // keys[3] is account 3's key but no validator: its account buys it
        // no standing.
        let (mut host, keys, _accounts) = share_host().await;
        let err = submit(
            &mut host,
            &keys[3],
            1,
            GovMsg::Propose {
                proposal_id: "no-standing".into(),
                action: GovAction::Signal {
                    text: "nope".into(),
                },
                voting_period: 20,
            },
        )
        .await
        .expect_err("an account key that is no validator cannot propose");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("member node")),
            "rejection names the missing membership, got {err:?}"
        );

        // a total stranger (no account, no seat) is refused the same way.
        let err = submit(
            &mut host,
            &key(200),
            2,
            GovMsg::Propose {
                proposal_id: "stranger".into(),
                action: GovAction::Signal {
                    text: "nope".into(),
                },
                voting_period: 20,
            },
        )
        .await
        .expect_err("a non-member stranger cannot propose");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("member node")),
            "rejection names the missing membership, got {err:?}"
        );

        // and neither can vote on an open proposal.
        submit(
            &mut host,
            &keys[0],
            3,
            GovMsg::Propose {
                proposal_id: "p".into(),
                action: GovAction::Signal { text: "x".into() },
                voting_period: 20,
            },
        )
        .await
        .expect("propose");
        let err = submit(
            &mut host,
            &keys[3],
            4,
            GovMsg::Vote {
                proposal_id: "p".into(),
                approve: true,
            },
        )
        .await
        .expect_err("a non-validator key holds no ballot");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module(ref m)) if m.contains("frozen electorate")),
            "rejection names the electorate, got {err:?}"
        );
    });
}
