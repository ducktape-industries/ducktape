use std::collections::BTreeMap;

use abi::{BlobId, HostOp, Message, Origin, Outcome, Scheme};
use borsh::{BorshDeserialize, BorshSerialize};
use commonware_cryptography::{Signer as _, ed25519};
use commonware_runtime::{Runner as _, deterministic};
use fixture_probe::Step;
use host::{Applied, Block, BlockId, Founding, Genesis, Host, Layer, Limits, Receipt, Submission};
use keyscheme::testkit;
use modules::{
    AccountNumber, Page, acl, attribution, capability, dispatch, gateway, governance, identity, kv,
    module_registry, reason, saga, valset,
};

macro_rules! program {
    ($name:literal) => {
        include_bytes!(concat!("../system/wasm/", $name, ".wasm"))
    };
}

const PROBE: &[u8] = include_bytes!("../../kernel/fixtures/wasm/fixture_probe.wasm");
const NETWORK: &[u8] = b"net";
const EPOCH_LENGTH: u64 = 4;
const TIME: u64 = 1_700_000_000_000;

type Ctx = deterministic::Context;

fn key(seed: u64) -> ed25519::PrivateKey {
    ed25519::PrivateKey::from_seed(seed)
}

fn public(seed: u64) -> Vec<u8> {
    key(seed).public_key().as_ref().to_vec()
}

fn member(seed: u64) -> valset::Member {
    valset::Member {
        key: public(seed),
        address: format!("v{seed}:1"),
    }
}

fn founding(program: &str, code: &[u8]) -> Founding {
    Founding {
        program: program.to_owned(),
        code: code.to_vec(),
        params: Vec::new(),
    }
}

fn block_id(height: u64) -> BlockId {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&height.to_be_bytes());
    id
}

struct Net {
    host: Host<Ctx>,
    height: u64,
    sequences: BTreeMap<Vec<u8>, u64>,
}

impl Net {
    async fn found(context: Ctx, dir: &std::path::Path) -> Net {
        let genesis = Genesis {
            network: NETWORK.to_vec(),
            module_registry: program!("module_registry").to_vec(),
            valset: program!("valset").to_vec(),
            validators: vec![member(1), member(2)],
            programs: vec![
                founding(kv::PROGRAM, program!("kv")),
                founding(acl::PROGRAM, program!("acl")),
                founding(identity::PROGRAM, program!("identity")),
                founding(governance::PROGRAM, program!("governance")),
                founding(capability::PROGRAM, program!("capability")),
                founding(saga::PROGRAM, program!("saga")),
                founding(dispatch::PROGRAM, program!("dispatch")),
                founding(attribution::PROGRAM, program!("attribution")),
                founding(gateway::PROGRAM, program!("gateway")),
                Founding {
                    program: "probe".into(),
                    code: PROBE.to_vec(),
                    params: abi::encode(&Vec::<Step>::new()),
                },
            ],
            limits: Limits::default(),
            epoch_length: EPOCH_LENGTH,
            time: TIME,
        };
        let (host, applied) = Host::found(context, "net", dir, block_id(0), genesis)
            .await
            .unwrap();
        for receipt in &applied.admissions {
            assert!(
                matches!(receipt.outcome, Outcome::Applied { .. }),
                "{} did not admit: {:?}",
                receipt.program,
                receipt.outcome
            );
        }
        Net {
            host,
            height: 0,
            sequences: BTreeMap::new(),
        }
    }

    fn time(&self) -> u64 {
        TIME + self.height * 1000
    }

    async fn block(&mut self, submissions: Vec<Submission>) -> Applied {
        self.height += 1;
        self.host
            .apply(Block {
                height: self.height,
                id: block_id(self.height),
                time: self.time(),
                submissions,
            })
            .await
            .unwrap()
    }

    async fn tick(&mut self) -> Applied {
        self.block(Vec::new()).await
    }

    async fn ticks(&mut self, blocks: u64) {
        for _ in 0..blocks {
            self.tick().await;
        }
    }

    fn submission(&self, signer: &[u8], target: &str, payload: Vec<u8>) -> Submission {
        Submission {
            signer: signer.to_vec(),
            seq: self.sequences.get(signer).copied().unwrap_or_default(),
            target: target.to_owned(),
            payload,
        }
    }

    fn consumed(&mut self, signer: &[u8], receipt: &Receipt) {
        if let Outcome::Applied { .. } = receipt.outcome {
            *self.sequences.entry(signer.to_vec()).or_default() += 1;
        }
    }

    async fn submit<T: BorshSerialize>(&mut self, signer: &[u8], target: &str, op: &T) -> Receipt {
        let submission = self.submission(signer, target, abi::encode(op));
        let applied = self.block(vec![submission]).await;
        let receipt = applied.submissions.into_iter().next().unwrap();
        self.consumed(signer, &receipt);
        receipt
    }

    async fn apply<T: BorshSerialize>(&mut self, signer: &[u8], target: &str, op: &T) -> Vec<u8> {
        let receipt = self.submit(signer, target, op).await;
        match receipt.outcome {
            Outcome::Applied { output } => output,
            Outcome::Rejected(refusal) => panic!("{target} rejected the op: {refusal}"),
        }
    }

    async fn refuse<T: BorshSerialize>(&mut self, signer: &[u8], target: &str, op: &T) -> String {
        let receipt = self.submit(signer, target, op).await;
        match receipt.outcome {
            Outcome::Applied { .. } => panic!("{target} applied the op"),
            Outcome::Rejected(refusal) => refusal.reason,
        }
    }

    async fn via_probe<T: BorshSerialize>(&mut self, target: &str, op: &T) -> Applied {
        let script = vec![Step::Op(HostOp::Emit(Message {
            target: target.to_owned(),
            payload: abi::encode(op),
            reply: false,
        }))];
        let submission = self.submission(&public(9), "probe", abi::encode(&script));
        let applied = self.block(vec![submission]).await;
        self.consumed(&public(9), &applied.submissions[0]);
        match &applied.submissions[0].outcome {
            Outcome::Applied { .. } => {}
            Outcome::Rejected(refusal) => panic!("the probe rejected: {refusal}"),
        }
        self.tick().await
    }

    async fn ask<Q: BorshSerialize, R: BorshDeserialize>(&self, program: &str, query: &Q) -> R {
        let answer = self
            .host
            .query(
                Layer::Confirmed,
                self.time(),
                Origin::External(public(1)),
                program,
                abi::encode(query),
            )
            .await
            .unwrap()
            .unwrap();
        abi::decode(&answer).unwrap()
    }

    async fn memberships(&self) -> Vec<valset::Membership> {
        match self.ask(valset::PROGRAM, &valset::Query::Memberships).await {
            valset::Reply::Memberships(memberships) => memberships,
            other => panic!("{other:?}"),
        }
    }

    async fn proposal(&self, id: &str) -> governance::Proposal {
        match self
            .ask(
                governance::PROGRAM,
                &governance::Query::Proposal { id: id.into() },
            )
            .await
        {
            governance::Reply::Proposal(Some(proposal)) => proposal,
            other => panic!("{other:?}"),
        }
    }

    async fn decide(
        &mut self,
        id: &str,
        action: governance::Action,
        voters: &[u64],
    ) -> governance::Proposal {
        let proposer = public(voters[0]);
        self.apply(
            &proposer,
            governance::PROGRAM,
            &governance::Op::Propose {
                id: id.into(),
                action,
                voting_blocks: 10,
            },
        )
        .await;
        for voter in voters {
            self.apply(
                &public(*voter),
                governance::PROGRAM,
                &governance::Op::Vote {
                    id: id.into(),
                    approve: true,
                },
            )
            .await;
        }
        self.apply(
            &proposer,
            governance::PROGRAM,
            &governance::Op::Execute { id: id.into() },
        )
        .await;
        self.ticks(2).await;
        self.proposal(id).await
    }
}

fn delivered_to<'a>(applied: &'a Applied, program: &str) -> Vec<&'a Receipt> {
    applied
        .deliveries
        .iter()
        .map(|delivered| &delivered.receipt)
        .filter(|receipt| receipt.program == program)
        .collect()
}

fn announced<'a>(receipts: impl IntoIterator<Item = &'a Receipt>) -> Vec<saga::Work> {
    receipts
        .into_iter()
        .filter(|receipt| receipt.program == saga::PROGRAM)
        .flat_map(|receipt| receipt.events.iter())
        .map(|event| abi::decode(event).unwrap())
        .collect()
}

fn work(applied: &Applied) -> Vec<saga::Work> {
    announced(
        applied.submissions.iter().chain(
            applied
                .deliveries
                .iter()
                .map(|delivered| &delivered.receipt),
        ),
    )
}

#[test]
fn founding_seats_the_validators_and_every_program_answers() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let net = Net::found(context, dir.path()).await;
        let programs = net.host.programs().unwrap();
        for program in [
            kv::PROGRAM,
            acl::PROGRAM,
            module_registry::PROGRAM,
            valset::PROGRAM,
            identity::PROGRAM,
            governance::PROGRAM,
            capability::PROGRAM,
            saga::PROGRAM,
            dispatch::PROGRAM,
            attribution::PROGRAM,
            gateway::PROGRAM,
        ] {
            assert!(programs.contains_key(program), "{program} is not rostered");
        }
        let memberships = net.memberships().await;
        assert_eq!(memberships.len(), 2);
        assert!(
            memberships
                .iter()
                .all(|membership| membership.standing == valset::Standing::Validator)
        );
        let seated = net.host.epoch_members(0).unwrap().unwrap();
        assert_eq!(seated.len(), 2);
        assert!(seated.contains(&member(1)));
        assert!(seated.contains(&member(2)));
        let acl::Reply::Policies(policies) = net.ask(acl::PROGRAM, &acl::Query::Policies).await
        else {
            panic!()
        };
        assert!(policies.is_empty());
    });
}

#[test]
fn kv_stores_lists_and_deletes() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        let anyone = public(7);
        net.apply(
            &anyone,
            kv::PROGRAM,
            &kv::Op::Set {
                key: b"a/1".to_vec(),
                value: b"one".to_vec(),
            },
        )
        .await;
        net.apply(
            &anyone,
            kv::PROGRAM,
            &kv::Op::Set {
                key: b"a/2".to_vec(),
                value: b"two".to_vec(),
            },
        )
        .await;
        let kv::Reply::Value(value) = net
            .ask(
                kv::PROGRAM,
                &kv::Query::Get {
                    key: b"a/1".to_vec(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(value, Some(b"one".to_vec()));
        let kv::Reply::Entries(entries) = net
            .ask(
                kv::PROGRAM,
                &kv::Query::List {
                    prefix: b"a/".to_vec(),
                    page: Page {
                        after: Some(b"a/1".to_vec()),
                        limit: None,
                    },
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, b"a/2");
        net.apply(
            &anyone,
            kv::PROGRAM,
            &kv::Op::Delete {
                key: b"a/1".to_vec(),
            },
        )
        .await;
        let kv::Reply::Value(value) = net
            .ask(
                kv::PROGRAM,
                &kv::Query::Get {
                    key: b"a/1".to_vec(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(value, None);
    });
}

#[test]
fn governance_tightens_acl_and_acl_gates_a_program() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        let lockout = net
            .refuse(
                &public(1),
                governance::PROGRAM,
                &governance::Op::Propose {
                    id: "lockout".into(),
                    action: governance::Action::SetPolicy {
                        target: acl::ANY.into(),
                        standing: Some(acl::Standing::User),
                    },
                    voting_blocks: 10,
                },
            )
            .await;
        assert_eq!(lockout, reason::CONFLICT);
        let proposal = net
            .decide(
                "kv-validators",
                governance::Action::SetPolicy {
                    target: kv::PROGRAM.into(),
                    standing: Some(acl::Standing::Validator),
                },
                &[1, 2],
            )
            .await;
        assert_eq!(
            proposal.status,
            governance::Status::Passed {
                effect: Some(governance::Effect::Applied)
            }
        );
        let acl::Reply::Required(required) = net
            .ask(
                acl::PROGRAM,
                &acl::Query::Required {
                    target: kv::PROGRAM.into(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(required, Some(acl::Standing::Validator));
        let set = kv::Op::Set {
            key: b"k".to_vec(),
            value: b"v".to_vec(),
        };
        assert_eq!(
            net.refuse(&public(7), kv::PROGRAM, &set).await,
            reason::UNAUTHORIZED
        );
        net.apply(&public(1), kv::PROGRAM, &set).await;
    });
}

#[test]
fn governance_admits_a_resident_and_promotes_it_into_the_next_epoch() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        let unmet = net
            .refuse(
                &public(1),
                governance::PROGRAM,
                &governance::Op::Propose {
                    id: "phantom".into(),
                    action: governance::Action::SetMembership(valset::Membership {
                        key: public(3),
                        address: "v3:1".into(),
                        standing: valset::Standing::Validator,
                    }),
                    voting_blocks: 10,
                },
            )
            .await;
        assert_eq!(unmet, reason::CONFLICT);
        let outsider = net
            .refuse(
                &public(3),
                governance::PROGRAM,
                &governance::Op::Propose {
                    id: "outsider".into(),
                    action: governance::Action::Signal { text: "hi".into() },
                    voting_blocks: 10,
                },
            )
            .await;
        assert_eq!(outsider, reason::UNAUTHORIZED);
        let admitted = net
            .decide(
                "admit-3",
                governance::Action::SetMembership(valset::Membership {
                    key: public(3),
                    address: "v3:1".into(),
                    standing: valset::Standing::Resident,
                }),
                &[1, 2],
            )
            .await;
        assert_eq!(
            admitted.status,
            governance::Status::Passed {
                effect: Some(governance::Effect::Applied)
            }
        );
        assert_eq!(admitted.electorate, governance::Electorate::Validators);
        assert_eq!(
            admitted.rule,
            governance::Rule::Threshold { required_yes: 2 }
        );
        let memberships = net.memberships().await;
        assert_eq!(memberships.len(), 3);
        let promoted = net
            .decide(
                "seat-3",
                governance::Action::SetMembership(valset::Membership {
                    key: public(3),
                    address: "v3:1".into(),
                    standing: valset::Standing::Validator,
                }),
                &[1, 2],
            )
            .await;
        assert_eq!(
            promoted.status,
            governance::Status::Passed {
                effect: Some(governance::Effect::Applied)
            }
        );
        let valset::Reply::Validators(validators) =
            net.ask(valset::PROGRAM, &valset::Query::Validators).await
        else {
            panic!()
        };
        assert_eq!(validators.len(), 3);
        let epoch = (net.height + EPOCH_LENGTH) / EPOCH_LENGTH;
        while net.host.epoch_members(epoch).unwrap().is_none() {
            net.tick().await;
        }
        let seated = net.host.epoch_members(epoch).unwrap().unwrap();
        assert_eq!(seated.len(), 3);
        net.apply(
            &public(1),
            governance::PROGRAM,
            &governance::Op::Propose {
                id: "remove-1".into(),
                action: governance::Action::RemoveMember { key: public(1) },
                voting_blocks: 10,
            },
        )
        .await;
        for voter in [1, 2, 3] {
            net.apply(
                &public(voter),
                governance::PROGRAM,
                &governance::Op::Vote {
                    id: "remove-1".into(),
                    approve: false,
                },
            )
            .await;
        }
        net.apply(
            &public(1),
            governance::PROGRAM,
            &governance::Op::Execute {
                id: "remove-1".into(),
            },
        )
        .await;
        assert_eq!(
            net.proposal("remove-1").await.status,
            governance::Status::Rejected
        );
    });
}

#[test]
fn an_invite_admits_a_resident_exactly_once() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        let issuer = key(1);
        let invite = |nonce: &[u8], expires_at: u64| {
            let grant = governance::Grant {
                network: NETWORK.to_vec(),
                nonce: nonce.to_vec(),
                expires_at,
            };
            governance::Invite {
                issuer: public(1),
                nonce: nonce.to_vec(),
                expires_at,
                signature: testkit::ed25519_proof(
                    &issuer,
                    governance::INVITE_NAMESPACE,
                    &grant.preimage(),
                ),
            }
        };
        let live = invite(b"nonce-1", TIME + 60_000);
        net.apply(
            &public(5),
            governance::PROGRAM,
            &governance::Op::Redeem {
                invite: live.clone(),
                address: "v5:1".into(),
            },
        )
        .await;
        net.tick().await;
        let joined = net
            .memberships()
            .await
            .into_iter()
            .find(|membership| membership.key == public(5))
            .unwrap();
        assert_eq!(joined.standing, valset::Standing::Resident);
        assert_eq!(joined.address, "v5:1");
        let again = net
            .refuse(
                &public(6),
                governance::PROGRAM,
                &governance::Op::Redeem {
                    invite: live,
                    address: "v6:1".into(),
                },
            )
            .await;
        assert_eq!(again, reason::CONFLICT);
        let stale = net
            .refuse(
                &public(6),
                governance::PROGRAM,
                &governance::Op::Redeem {
                    invite: invite(b"nonce-2", TIME - 1),
                    address: "v6:1".into(),
                },
            )
            .await;
        assert_eq!(stale, reason::UNAUTHORIZED);
        let mut forged = invite(b"nonce-3", TIME + 60_000);
        forged.expires_at += 1;
        let forged = net
            .refuse(
                &public(6),
                governance::PROGRAM,
                &governance::Op::Redeem {
                    invite: forged,
                    address: "v6:1".into(),
                },
            )
            .await;
        assert_eq!(forged, reason::UNAUTHORIZED);
        let governance::Reply::Redemption(Some(redemption)) = net
            .ask(
                governance::PROGRAM,
                &governance::Query::Redemption {
                    nonce: b"nonce-1".to_vec(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(redemption.joiner, public(5));
    });
}

#[test]
fn a_published_program_is_scheduled_by_governance_and_seated_at_its_height() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        let output = net
            .apply(
                &public(1),
                module_registry::PROGRAM,
                &module_registry::Op::Publish {
                    body: program!("kv").to_vec(),
                },
            )
            .await;
        let code: BlobId = abi::decode(&output).unwrap();
        let entry = module_registry::Entry {
            program: "kv2".into(),
            code,
            params: Vec::new(),
        };
        let unpublished = net
            .refuse(
                &public(1),
                governance::PROGRAM,
                &governance::Op::Propose {
                    id: "ghost".into(),
                    action: governance::Action::ScheduleProgram {
                        lead: 2,
                        change: module_registry::Change::Set(module_registry::Entry {
                            program: "ghost".into(),
                            code: BlobId::Sha256([9; 32]),
                            params: Vec::new(),
                        }),
                    },
                    voting_blocks: 10,
                },
            )
            .await;
        assert_eq!(unpublished, reason::NOT_FOUND);
        let proposal = net
            .decide(
                "add-kv2",
                governance::Action::ScheduleProgram {
                    lead: 2,
                    change: module_registry::Change::Set(entry.clone()),
                },
                &[1, 2],
            )
            .await;
        assert_eq!(
            proposal.status,
            governance::Status::Passed {
                effect: Some(governance::Effect::Applied)
            }
        );
        let module_registry::Reply::Scheduled(scheduled) = net
            .ask(module_registry::PROGRAM, &module_registry::Query::Scheduled)
            .await
        else {
            panic!()
        };
        assert_eq!(scheduled.len(), 1);
        assert_eq!(
            scheduled[0].change,
            module_registry::Change::Set(entry.clone())
        );
        let lands_at = scheduled[0].height;
        assert!(lands_at > net.height);
        while net.height + 1 < lands_at {
            let applied = net.tick().await;
            assert!(applied.admissions.is_empty());
        }
        let applied = net.tick().await;
        assert_eq!(applied.height, lands_at);
        assert_eq!(applied.admissions.len(), 1);
        assert_eq!(applied.admissions[0].program, "kv2");
        assert!(net.host.programs().unwrap().contains_key("kv2"));
        net.apply(
            &public(1),
            "kv2",
            &kv::Op::Set {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
        )
        .await;
        let removal = net
            .decide(
                "drop-kv2",
                governance::Action::ScheduleProgram {
                    lead: 1,
                    change: module_registry::Change::Remove("kv2".into()),
                },
                &[1, 2],
            )
            .await;
        assert!(matches!(removal.status, governance::Status::Passed { .. }));
        net.ticks(3).await;
        assert!(!net.host.programs().unwrap().contains_key("kv2"));
    });
}

fn consent(
    authorizer: &ed25519::PrivateKey,
    account: AccountNumber,
    new_key: &[u8],
    generation: u64,
    expires_at: u64,
) -> identity::Consent {
    let admission = identity::Admission {
        network: NETWORK.to_vec(),
        scheme: Scheme::Ed25519,
        key: new_key.to_vec(),
        generation,
        account,
        expires_at,
    };
    identity::Consent {
        key: authorizer.public_key().as_ref().to_vec(),
        account,
        expires_at,
        proof: testkit::ed25519_proof(
            authorizer,
            identity::CONSENT_NAMESPACE,
            &admission.preimage(),
        ),
    }
}

#[test]
fn identity_founds_accounts_admits_keys_by_consent_and_provisions_programs() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        let output = net
            .apply(
                &public(1),
                identity::PROGRAM,
                &identity::Op::Create {
                    name: " Alice ".into(),
                    scheme: Scheme::Ed25519,
                },
            )
            .await;
        assert_eq!(abi::decode::<AccountNumber>(&output).unwrap(), 1);
        let twice = net
            .refuse(
                &public(1),
                identity::PROGRAM,
                &identity::Op::Create {
                    name: "Alice again".into(),
                    scheme: Scheme::Ed25519,
                },
            )
            .await;
        assert_eq!(twice, reason::CONFLICT);
        let phone = public(11);
        let expires_at = TIME + 600_000;
        net.apply(
            &phone,
            identity::PROGRAM,
            &identity::Op::AddKey {
                scheme: Scheme::Ed25519,
                label: Some("phone".into()),
                consent: consent(&key(1), 1, &phone, 0, expires_at),
            },
        )
        .await;
        let identity::Reply::Account(Some(account)) = net
            .ask(identity::PROGRAM, &identity::Query::Get { number: 1 })
            .await
        else {
            panic!()
        };
        assert_eq!(account.name, "Alice");
        assert_eq!(account.keys().len(), 2);
        let identity::Reply::Number(of_phone) = net
            .ask(
                identity::PROGRAM,
                &identity::Query::OfKey { key: phone.clone() },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(of_phone, Some(1));
        let replayed = net
            .refuse(
                &public(12),
                identity::PROGRAM,
                &identity::Op::AddKey {
                    scheme: Scheme::Ed25519,
                    label: None,
                    consent: consent(&key(1), 1, &phone, 0, expires_at),
                },
            )
            .await;
        assert_eq!(replayed, reason::UNAUTHORIZED);
        let senior = net
            .refuse(
                &phone,
                identity::PROGRAM,
                &identity::Op::RemoveKey { key: public(1) },
            )
            .await;
        assert_eq!(senior, reason::UNAUTHORIZED);
        net.apply(
            &public(1),
            identity::PROGRAM,
            &identity::Op::RemoveKey { key: phone.clone() },
        )
        .await;
        let last = net
            .refuse(
                &public(1),
                identity::PROGRAM,
                &identity::Op::RemoveKey { key: public(1) },
            )
            .await;
        assert_eq!(last, reason::CONFLICT);
        let identity::Reply::Generation(generation) = net
            .ask(
                identity::PROGRAM,
                &identity::Query::Generation { key: phone.clone() },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(generation, 1);
        net.apply(
            &phone,
            identity::PROGRAM,
            &identity::Op::AddKey {
                scheme: Scheme::Ed25519,
                label: Some("phone again".into()),
                consent: consent(&key(1), 1, &phone, 1, expires_at),
            },
        )
        .await;
        let applied = net
            .via_probe(
                identity::PROGRAM,
                &identity::Op::CreateProgram {
                    name: "Chief".into(),
                    controller: 1,
                },
            )
            .await;
        let created = delivered_to(&applied, identity::PROGRAM)[0];
        let Outcome::Applied { output } = &created.outcome else {
            panic!("{:?}", created.outcome)
        };
        assert_eq!(abi::decode::<AccountNumber>(output).unwrap(), 2);
        let identity::Reply::Account(Some(chief)) = net
            .ask(identity::PROGRAM, &identity::Query::Get { number: 2 })
            .await
        else {
            panic!()
        };
        assert_eq!(
            chief.control,
            identity::Control::Program {
                executor: "probe".into(),
                controller: 1,
                standing: identity::Standing::Active,
            }
        );
        let identity::Reply::Accounts(controlled) = net
            .ask(
                identity::PROGRAM,
                &identity::Query::Controlled {
                    by: 1,
                    page: Page::all(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(controlled.len(), 1);
        assert_eq!(controlled[0].number, 2);
        let not_the_executor = net
            .refuse(
                &public(1),
                identity::PROGRAM,
                &identity::Op::SetStanding {
                    account: 2,
                    standing: identity::Standing::Suspended,
                },
            )
            .await;
        assert_eq!(not_the_executor, reason::UNAUTHORIZED);
        net.apply(
            &public(1),
            identity::PROGRAM,
            &identity::Op::SetName {
                account: 1,
                name: "Alice B".into(),
            },
        )
        .await;
        let circular = net
            .refuse(
                &public(1),
                identity::PROGRAM,
                &identity::Op::TransferControl { account: 2, to: 2 },
            )
            .await;
        assert_eq!(circular, reason::CONFLICT);
        net.apply(
            &public(1),
            identity::PROGRAM,
            &identity::Op::Revoke { account: 2 },
        )
        .await;
        let identity::Reply::Account(Some(revoked)) = net
            .ask(identity::PROGRAM, &identity::Query::Get { number: 2 })
            .await
        else {
            panic!()
        };
        assert_eq!(
            revoked.control,
            identity::Control::Revoked { controller: 1 }
        );
    });
}

#[test]
fn capability_registers_members_and_classes() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        let announcement = capability::Announcement {
            tags: vec!["codex".into(), "claude".into()],
            resources: BTreeMap::from([("cores".to_owned(), 8)]),
        };
        let stranger = net
            .refuse(
                &public(7),
                capability::PROGRAM,
                &capability::Op::Announce(announcement.clone()),
            )
            .await;
        assert_eq!(stranger, reason::UNAUTHORIZED);
        net.apply(
            &public(2),
            capability::PROGRAM,
            &capability::Op::Announce(announcement.clone()),
        )
        .await;
        let junk = net
            .refuse(
                &public(1),
                capability::PROGRAM,
                &capability::Op::Announce(capability::Announcement {
                    tags: vec!["Codex!".into()],
                    resources: BTreeMap::new(),
                }),
            )
            .await;
        assert_eq!(junk, reason::INVALID_INPUT);
        let capability::Reply::Providers(providers) = net
            .ask(
                capability::PROGRAM,
                &capability::Query::Providers {
                    tag: "codex".into(),
                    demands: BTreeMap::from([("cores".to_owned(), 4)]),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(providers, vec![public(2)]);
        let capability::Reply::Providers(too_demanding) = net
            .ask(
                capability::PROGRAM,
                &capability::Query::Providers {
                    tag: "codex".into(),
                    demands: BTreeMap::from([("cores".to_owned(), 16)]),
                },
            )
            .await
        else {
            panic!()
        };
        assert!(too_demanding.is_empty());
        net.via_probe(
            capability::PROGRAM,
            &capability::Op::ClaimClass {
                class: "agent".into(),
            },
        )
        .await;
        let capability::Reply::Class(owner) = net
            .ask(
                capability::PROGRAM,
                &capability::Query::Class {
                    class: "agent".into(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(owner, Some("probe".into()));
        assert_eq!(capability::class_of("agent:chief"), Some("agent"));
        assert_eq!(capability::class_of("codex"), None);
    });
}

#[test]
fn a_saga_leases_work_to_a_provider_and_settles_on_its_result() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        net.apply(
            &public(2),
            capability::PROGRAM,
            &capability::Op::Announce(capability::Announcement {
                tags: vec!["codex".into()],
                resources: BTreeMap::new(),
            }),
        )
        .await;
        let id = saga::id_for(&Origin::External(public(1)), "job-1");
        let trigger = |id: &str, attempts: u32, lease: Option<u64>| {
            saga::Op::Trigger(saga::Trigger {
                id: id.to_owned(),
                spec: b"run it".to_vec(),
                reply_to: None,
                correlation: Vec::new(),
                deadline: None,
                attempts,
                lease,
                capability: Some("codex".into()),
                demands: BTreeMap::new(),
                pinned: None,
            })
        };
        let squatted = net
            .refuse(&public(7), saga::PROGRAM, &trigger(&id, 1, None))
            .await;
        assert_eq!(squatted, reason::UNAUTHORIZED);
        let receipt = net
            .submit(&public(1), saga::PROGRAM, &trigger(&id, 1, None))
            .await;
        assert!(matches!(receipt.outcome, Outcome::Applied { .. }));
        let first: saga::Work = abi::decode(&receipt.events[0]).unwrap();
        assert_eq!(first.assignee, Some(public(2)));
        assert_eq!(first.attempt, 1);
        let saga::Reply::Work(assigned) = net
            .ask(
                saga::PROGRAM,
                &saga::Query::Assigned {
                    node: public(2),
                    page: Page::all(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(assigned, vec![first]);
        let stranger = net
            .refuse(
                &public(1),
                saga::PROGRAM,
                &saga::Op::Result {
                    id: id.clone(),
                    attempt: 1,
                    outcome: Ok(b"done".to_vec()),
                    usage: saga::Usage::default(),
                },
            )
            .await;
        assert_eq!(stranger, reason::UNAUTHORIZED);
        net.apply(
            &public(2),
            saga::PROGRAM,
            &saga::Op::Result {
                id: id.clone(),
                attempt: 1,
                outcome: Ok(b"done".to_vec()),
                usage: saga::Usage {
                    input_tokens: 10,
                    ..Default::default()
                },
            },
        )
        .await;
        let saga::Reply::Saga(Some(settled)) = net
            .ask(saga::PROGRAM, &saga::Query::Get { id: id.clone() })
            .await
        else {
            panic!()
        };
        assert_eq!(
            settled.status,
            saga::Status::Settled(saga::Outcome::Done(b"done".to_vec()))
        );
        assert_eq!(settled.usage.input_tokens, 10);
        let leased = saga::id_for(&Origin::External(public(1)), "job-2");
        net.apply(&public(1), saga::PROGRAM, &trigger(&leased, 2, Some(2)))
            .await;
        let saga::Reply::NextExpiry(expiry) =
            net.ask(saga::PROGRAM, &saga::Query::NextExpiry).await
        else {
            panic!()
        };
        assert_eq!(expiry, Some(net.height + 2));
        let early = net
            .submit(&public(7), saga::PROGRAM, &saga::Op::Crank)
            .await;
        assert!(early.events.is_empty());
        let expired = net
            .submit(&public(7), saga::PROGRAM, &saga::Op::Crank)
            .await;
        let retried = announced([&expired]);
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].attempt, 2);
        net.ticks(2).await;
        net.submit(&public(7), saga::PROGRAM, &saga::Op::Crank)
            .await;
        let saga::Reply::Saga(Some(failed)) = net
            .ask(saga::PROGRAM, &saga::Query::Get { id: leased.clone() })
            .await
        else {
            panic!()
        };
        assert_eq!(
            failed.status,
            saga::Status::Settled(saga::Outcome::Failed("the lease expired".into()))
        );
        net.apply(
            &public(1),
            saga::PROGRAM,
            &saga::Op::Prune {
                ids: vec![leased.clone()],
            },
        )
        .await;
        let saga::Reply::Saga(pruned) = net
            .ask(saga::PROGRAM, &saga::Query::Get { id: leased })
            .await
        else {
            panic!()
        };
        assert_eq!(pruned, None);
    });
}

#[test]
fn a_dispatch_runs_a_recipe_through_saga_and_delivers_the_judged_result() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        net.apply(
            &public(2),
            capability::PROGRAM,
            &capability::Op::Announce(capability::Announcement {
                tags: vec!["codex".into()],
                resources: BTreeMap::new(),
            }),
        )
        .await;
        let manifest = dispatch::Manifest {
            description: "review a diff".into(),
            capability: "codex".into(),
            routing: dispatch::Routing::Capability,
            contract: dispatch::Contract::Json,
            attempts: 1,
            deadline: None,
            lease: None,
        };
        net.apply(
            &public(1),
            dispatch::PROGRAM,
            &dispatch::Op::SetRecipe {
                id: "review".into(),
                manifest: manifest.clone(),
            },
        )
        .await;
        let foreign = net
            .refuse(
                &public(2),
                dispatch::PROGRAM,
                &dispatch::Op::RemoveRecipe {
                    id: "review".into(),
                },
            )
            .await;
        assert_eq!(foreign, reason::UNAUTHORIZED);
        let external = net
            .refuse(
                &public(1),
                dispatch::PROGRAM,
                &dispatch::Op::Dispatch {
                    id: "d1".into(),
                    recipe: "review".into(),
                    payload: b"diff".to_vec(),
                    demands: BTreeMap::new(),
                    admission: dispatch::Admission::Queue,
                },
            )
            .await;
        assert_eq!(external, reason::UNAUTHORIZED);
        let applied = net
            .via_probe(
                dispatch::PROGRAM,
                &dispatch::Op::Dispatch {
                    id: "d1".into(),
                    recipe: "review".into(),
                    payload: b"diff".to_vec(),
                    demands: BTreeMap::new(),
                    admission: dispatch::Admission::Queue,
                },
            )
            .await;
        assert!(matches!(
            delivered_to(&applied, dispatch::PROGRAM)[0].outcome,
            Outcome::Applied { .. }
        ));
        let applied = net.tick().await;
        let announced = work(&applied);
        assert_eq!(announced.len(), 1);
        let saga_id = dispatch::saga_id("probe", "d1");
        assert_eq!(announced[0].id, saga_id);
        assert_eq!(announced[0].assignee, Some(public(2)));
        let spec: dispatch::Spec = abi::decode(&announced[0].spec).unwrap();
        assert_eq!(spec.receiver, "probe");
        assert_eq!(spec.payload, b"diff");
        net.apply(
            &public(2),
            saga::PROGRAM,
            &saga::Op::Result {
                id: saga_id.clone(),
                attempt: 1,
                outcome: Ok(b"{\"verdict\":\"ok\"}".to_vec()),
                usage: saga::Usage::default(),
            },
        )
        .await;
        net.ticks(2).await;
        let dispatch::Reply::Dispatch(Some(delivered)) = net
            .ask(
                dispatch::PROGRAM,
                &dispatch::Query::Dispatch {
                    receiver: "probe".into(),
                    id: "d1".into(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(
            delivered.status,
            dispatch::Status::Delivered {
                outcome: Ok(b"{\"verdict\":\"ok\"}".to_vec())
            }
        );
        net.via_probe(
            dispatch::PROGRAM,
            &dispatch::Op::Dispatch {
                id: "d2".into(),
                recipe: "review".into(),
                payload: b"diff".to_vec(),
                demands: BTreeMap::new(),
                admission: dispatch::Admission::Queue,
            },
        )
        .await;
        net.tick().await;
        net.apply(
            &public(2),
            saga::PROGRAM,
            &saga::Op::Result {
                id: dispatch::saga_id("probe", "d2"),
                attempt: 1,
                outcome: Ok(b"not json".to_vec()),
                usage: saga::Usage::default(),
            },
        )
        .await;
        net.ticks(2).await;
        let dispatch::Reply::Dispatch(Some(judged)) = net
            .ask(
                dispatch::PROGRAM,
                &dispatch::Query::Dispatch {
                    receiver: "probe".into(),
                    id: "d2".into(),
                },
            )
            .await
        else {
            panic!()
        };
        let dispatch::Status::Delivered {
            outcome: Err(error),
        } = judged.status
        else {
            panic!("{:?}", judged.status)
        };
        assert!(
            error.starts_with("the output is not one JSON value"),
            "{error}"
        );
    });
}

#[test]
fn attribution_records_changes_and_transfers_and_refuses_a_stale_revision() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        let object = attribution::Object {
            kind: "page".into(),
            id: "p1".into(),
        };
        let relation =
            |recipient: AccountNumber, reason: attribution::Reason| attribution::Relation {
                recipient,
                reason,
                detail: Vec::new(),
            };
        net.via_probe(
            attribution::PROGRAM,
            &attribution::Op::Attribute(attribution::Update {
                object: object.clone(),
                revision: 1,
                actor: attribution::Actor::Account(1),
                relations: vec![
                    relation(1, attribution::Reason::Ownership),
                    relation(2, attribution::Reason::Mention),
                ],
                transfers: Vec::new(),
            }),
        )
        .await;
        let applied = net
            .via_probe(
                attribution::PROGRAM,
                &attribution::Op::Attribute(attribution::Update {
                    object: object.clone(),
                    revision: 2,
                    actor: attribution::Actor::Account(1),
                    relations: vec![relation(3, attribution::Reason::Ownership)],
                    transfers: vec![attribution::Transfer {
                        reason: attribution::Reason::Ownership,
                        from: 1,
                        to: 3,
                    }],
                }),
            )
            .await;
        assert!(matches!(
            delivered_to(&applied, attribution::PROGRAM)[0].outcome,
            Outcome::Applied { .. }
        ));
        let applied = net
            .via_probe(
                attribution::PROGRAM,
                &attribution::Op::Attribute(attribution::Update {
                    object: object.clone(),
                    revision: 2,
                    actor: attribution::Actor::System,
                    relations: Vec::new(),
                    transfers: Vec::new(),
                }),
            )
            .await;
        let Outcome::Rejected(stale) = &delivered_to(&applied, attribution::PROGRAM)[0].outcome
        else {
            panic!()
        };
        assert_eq!(stale.reason, reason::CONFLICT);
        let attribution::Reply::Changes(changes) = net
            .ask(
                attribution::PROGRAM,
                &attribution::Query::Changes { page: Page::all() },
            )
            .await
        else {
            panic!()
        };
        let kinds: Vec<(AccountNumber, attribution::Kind)> = changes
            .iter()
            .map(|change| (change.recipient, change.kind.clone()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (1, attribution::Kind::Added),
                (2, attribution::Kind::Added),
                (1, attribution::Kind::TransferredOut { to: 3 }),
                (2, attribution::Kind::Withdrawn),
                (3, attribution::Kind::TransferredIn { from: 1 }),
            ]
        );
        assert_eq!(changes[0].source.program, "probe");
        let attribution::Reply::Changes(to_three) = net
            .ask(
                attribution::PROGRAM,
                &attribution::Query::ChangesTo {
                    recipient: 3,
                    page: Page::all(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(to_three.len(), 1);
        assert_eq!(to_three[0].seq, 5);
        let attribution::Reply::Relations(Some(relations)) = net
            .ask(
                attribution::PROGRAM,
                &attribution::Query::Relations {
                    source: attribution::Source {
                        program: "probe".into(),
                        object,
                    },
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(relations.revision, 2);
        assert_eq!(relations.relations.len(), 1);
    });
}

#[test]
fn gateway_binds_handles_routes_and_credentials_to_accounts() {
    deterministic::Runner::default().start(|context| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut net = Net::found(context, dir.path()).await;
        let no_account = net
            .refuse(
                &public(1),
                gateway::PROGRAM,
                &gateway::Op::SetHandle {
                    handle: Some("alice".into()),
                },
            )
            .await;
        assert_eq!(no_account, reason::UNAUTHORIZED);
        for (seed, name) in [(1, "Alice"), (2, "Bob")] {
            net.apply(
                &public(seed),
                identity::PROGRAM,
                &identity::Op::Create {
                    name: name.into(),
                    scheme: Scheme::Ed25519,
                },
            )
            .await;
        }
        net.apply(
            &public(1),
            gateway::PROGRAM,
            &gateway::Op::SetHandle {
                handle: Some("alice".into()),
            },
        )
        .await;
        let taken = net
            .refuse(
                &public(2),
                gateway::PROGRAM,
                &gateway::Op::SetHandle {
                    handle: Some("alice".into()),
                },
            )
            .await;
        assert_eq!(taken, reason::CONFLICT);
        let gateway::Reply::Resolved(resolved) = net
            .ask(
                gateway::PROGRAM,
                &gateway::Query::Resolve {
                    handle: "alice".into(),
                },
            )
            .await
        else {
            panic!()
        };
        assert_eq!(resolved, Some(1));
        let definition = gateway::Definition {
            publisher: public(1),
            target: gateway::Target::Loopback,
            policy: gateway::Policy {
                audience: gateway::Audience::Accounts(vec![2]),
                methods: vec![gateway::Method::Get, gateway::Method::Post],
                max_request_bytes: None,
                max_response_bytes: None,
                allow_authorization: false,
                allow_upgrade: true,
            },
        };
        net.apply(
            &public(1),
            gateway::PROGRAM,
            &gateway::Op::SetRoute {
                name: Some("blog".into()),
                definition: Some(definition.clone()),
            },
        )
        .await;
        let unordered = net
            .refuse(
                &public(1),
                gateway::PROGRAM,
                &gateway::Op::SetRoute {
                    name: None,
                    definition: Some(gateway::Definition {
                        policy: gateway::Policy {
                            methods: vec![gateway::Method::Post, gateway::Method::Get],
                            ..definition.policy.clone()
                        },
                        ..definition.clone()
                    }),
                },
            )
            .await;
        assert_eq!(unordered, reason::INVALID_INPUT);
        let gateway::Reply::Routes(routes) = net
            .ask(gateway::PROGRAM, &gateway::Query::Routes { account: 1 })
            .await
        else {
            panic!()
        };
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].name.as_deref(), Some("blog"));
        assert_eq!(routes[0].definition, definition);
        net.apply(
            &public(1),
            gateway::PROGRAM,
            &gateway::Op::SetCredential {
                name: "claude".into(),
                kind: gateway::CredentialKind::Claude,
                publisher: public(1),
                seal_key: [3; 32],
            },
        )
        .await;
        net.apply(
            &public(1),
            gateway::PROGRAM,
            &gateway::Op::GrantCredential {
                name: "claude".into(),
                to: 2,
            },
        )
        .await;
        let gateway::Reply::Credentials(granted) = net
            .ask(gateway::PROGRAM, &gateway::Query::Granted { to: 2 })
            .await
        else {
            panic!()
        };
        assert_eq!(granted.len(), 1);
        assert_eq!(granted[0].grants, vec![2]);
        net.apply(
            &public(1),
            gateway::PROGRAM,
            &gateway::Op::RevokeCredential {
                name: "claude".into(),
                from: 2,
            },
        )
        .await;
        let gateway::Reply::Credentials(revoked) = net
            .ask(gateway::PROGRAM, &gateway::Query::Granted { to: 2 })
            .await
        else {
            panic!()
        };
        assert!(revoked.is_empty());
    });
}
