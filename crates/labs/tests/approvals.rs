//! The approval path: who may propose, who may approve, what the threshold
//! counts, and what a byzantine participant cannot do.

use alloy_primitives::{Address, B256, U256, keccak256};
use futures::executor::block_on;
use labs::multisig::safe::{SafeTx, safe_tx_hash};
use labs::multisig::{
    ExecutableView, Multisig, MultisigEvent, MultisigMsg, MultisigQuery, MultisigReply,
    bind_preimage, decode_event, decode_reply, encode_msg, encode_query, register_preimage,
};
use sdk::{Env, Error, Module, Msg, Origin};
use valset::{ValsetQuery, ValsetReply};

const VAULT: &str = "treasury";
const CHAIN: u64 = 1;

// ---- harness ---------------------------------------------------------------

use sdk_testkit::TestCtx;

/// a valset-query responder: Validators from the given set, Residents empty —
/// the only host-routed read the multisig member gate makes.
fn valset_reads(validators: Vec<Vec<u8>>) -> impl FnMut(&[u8]) -> Result<Vec<u8>, Error> {
    move |req| match valset::decode_query(req).map_err(|e| Error::module("codec", e))? {
        ValsetQuery::Validators => Ok(valset::encode_reply(&ValsetReply::Validators(
            validators.clone(),
        ))),
        ValsetQuery::Residents => Ok(valset::encode_reply(&ValsetReply::Residents(vec![]))),
        ValsetQuery::MeshWindow => Ok(valset::encode_reply(&ValsetReply::MeshWindow(vec![]))),
    }
}

fn ctx(who: &[u8], validators: Vec<Vec<u8>>) -> TestCtx {
    TestCtx::with_env(Env {
        height: 1,
        consensus_time: 1_000,
        origin: Origin::External(who.to_vec()),
        me: "multisig".into(),
        cause: sdk::Cause::Direct,
    })
    .on_query("valset", valset_reads(validators))
}

/// One vault owner: a secp256k1 key, exactly as the desktop derives it from the
/// mnemonic seed.
struct Owner(k256::ecdsa::SigningKey);

impl Owner {
    fn new(seed: u8) -> Self {
        Self(k256::ecdsa::SigningKey::from_slice(&[seed; 32]).expect("valid scalar"))
    }
    fn address(&self) -> Address {
        let pk = self.0.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&pk.as_bytes()[1..])[12..])
    }
    fn sign(&self, digest: B256) -> Vec<u8> {
        let (sig, recid) = self
            .0
            .sign_prehash_recoverable(digest.as_slice())
            .expect("sign");
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = recid.to_byte();
        out.to_vec()
    }
    fn sign_preimage(&self, preimage: &[u8]) -> Vec<u8> {
        self.sign(keccak256(preimage))
    }
}

fn safe_addr() -> Address {
    Address::from([0x5au8; 20])
}

async fn exec(m: &mut Multisig, c: &mut TestCtx, msg: MultisigMsg) -> Result<(), Error> {
    m.execute(
        c,
        &Msg {
            target: "multisig".into(),
            payload: encode_msg(&msg),
        },
    )
    .await?;
    m.commit_block().await
}

/// Register a 2-of-3 vault owned by a, b, c.
async fn register(m: &mut Multisig, owners: &[&Owner], threshold: u8, signer: &Owner) {
    let owner_bytes: Vec<Vec<u8>> = owners.iter().map(|o| o.address().to_vec()).collect();
    let safe = safe_addr().to_vec();
    let preimage = register_preimage(VAULT, CHAIN, &safe, &owner_bytes, threshold);
    let mut c = ctx(b"node", vec![]);
    exec(
        m,
        &mut c,
        MultisigMsg::RegisterVault {
            vault_id: VAULT.into(),
            chain_id: CHAIN,
            safe_address: safe,
            owners: owner_bytes,
            threshold,
            signature: signer.sign_preimage(&preimage),
        },
    )
    .await
    .expect("register");
}

fn hash_for(to: Address, value: u64, data: Vec<u8>, nonce: u64) -> B256 {
    safe_tx_hash(
        CHAIN,
        safe_addr(),
        &SafeTx::call(to, U256::from(value), data, nonce),
    )
}

fn propose_msg(owner: &Owner, to: Address, value: u64, nonce: u64) -> MultisigMsg {
    let hash = hash_for(to, value, vec![], nonce);
    MultisigMsg::ProposeTx {
        vault_id: VAULT.into(),
        nonce,
        to: to.to_vec(),
        value: U256::from(value).to_be_bytes::<32>().to_vec(),
        data: vec![],
        signature: owner.sign(hash),
    }
}

async fn executables(m: &Multisig) -> Vec<ExecutableView> {
    let reply = m
        .query(&encode_query(&MultisigQuery::Executable {
            vault_id: VAULT.into(),
        }))
        .await
        .expect("query");
    match decode_reply(&reply).expect("decode") {
        MultisigReply::Executable(v) => v,
        other => panic!("unexpected reply: {other:?}"),
    }
}

// ---- tests -----------------------------------------------------------------

#[test]
fn two_of_three_needs_two_distinct_owners() {
    block_on(async {
        let (a, b, c) = (Owner::new(1), Owner::new(2), Owner::new(3));
        let mut m = Multisig::new("multisig", "valset");
        register(&mut m, &[&a, &b, &c], 2, &a).await;

        let to = Address::from([0x11u8; 20]);
        let mut cx = ctx(b"node", vec![]);
        exec(&mut m, &mut cx, propose_msg(&a, to, 100, 0))
            .await
            .expect("propose");

        // one approval (the proposer's own) is not a threshold
        assert!(executables(&m).await.is_empty());
        assert!(
            cx.events().is_empty(),
            "no executable event before the threshold"
        );

        // the SECOND owner crosses it
        let hash = hash_for(to, 100, vec![], 0);
        let mut cx = ctx(b"node", vec![]);
        exec(
            &mut m,
            &mut cx,
            MultisigMsg::Approve {
                vault_id: VAULT.into(),
                safe_tx_hash: hash.to_vec(),
                signature: b.sign(hash),
            },
        )
        .await
        .expect("approve");

        let ready = executables(&m).await;
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].safe_tx_hash, hash.to_vec());
        assert!(!ready[0].calldata.is_empty());

        // the threshold-crossing approval emits the broadcast cue
        assert_eq!(cx.events().len(), 1);
        match decode_event(&cx.events()[0].payload).expect("decode event") {
            MultisigEvent::Executable(e) => assert_eq!(e.safe_tx_hash, hash.to_vec()),
        }
    });
}

/// The single most important property: one owner cannot reach a 2-of-3
/// threshold alone, no matter how many approvals they submit. Approvals are
/// keyed by RECOVERED owner, so re-submitting is idempotent.
#[test]
fn one_owner_cannot_reach_the_threshold_alone() {
    block_on(async {
        let (a, b, c) = (Owner::new(1), Owner::new(2), Owner::new(3));
        let mut m = Multisig::new("multisig", "valset");
        register(&mut m, &[&a, &b, &c], 2, &a).await;

        let to = Address::from([0x11u8; 20]);
        let mut cx = ctx(b"node", vec![]);
        exec(&mut m, &mut cx, propose_msg(&a, to, 100, 0))
            .await
            .expect("propose");

        let hash = hash_for(to, 100, vec![], 0);
        // the same owner approving five more times changes nothing
        for _ in 0..5 {
            let mut cx = ctx(b"node", vec![]);
            exec(
                &mut m,
                &mut cx,
                MultisigMsg::Approve {
                    vault_id: VAULT.into(),
                    safe_tx_hash: hash.to_vec(),
                    signature: a.sign(hash),
                },
            )
            .await
            .expect("re-approve is a no-op, not an error");
        }
        assert!(
            executables(&m).await.is_empty(),
            "one owner must never reach a 2-of-3 threshold"
        );
    });
}

#[test]
fn a_non_owner_can_neither_propose_nor_approve() {
    block_on(async {
        let (a, b, c) = (Owner::new(1), Owner::new(2), Owner::new(3));
        let stranger = Owner::new(9);
        let mut m = Multisig::new("multisig", "valset");
        register(&mut m, &[&a, &b, &c], 2, &a).await;

        let to = Address::from([0x11u8; 20]);
        let mut cx = ctx(b"node", vec![]);
        let err = exec(&mut m, &mut cx, propose_msg(&stranger, to, 100, 0))
            .await
            .expect_err("a stranger must not propose");
        assert!(format!("{err:?}").contains("current owner"));

        // a real proposal, then a stranger's approval on top
        let mut cx = ctx(b"node", vec![]);
        exec(&mut m, &mut cx, propose_msg(&a, to, 100, 0))
            .await
            .expect("propose");
        let hash = hash_for(to, 100, vec![], 0);
        let mut cx = ctx(b"node", vec![]);
        let err = exec(
            &mut m,
            &mut cx,
            MultisigMsg::Approve {
                vault_id: VAULT.into(),
                safe_tx_hash: hash.to_vec(),
                signature: stranger.sign(hash),
            },
        )
        .await
        .expect_err("a stranger must not approve");
        assert!(format!("{err:?}").contains("current owner"));
        assert!(executables(&m).await.is_empty());
    });
}

/// An approval is a signature over ONE SafeTx hash. It must not carry to a
/// different transaction — the property that makes "sign what you were shown"
/// meaningful.
#[test]
fn an_approval_does_not_transfer_to_another_proposal() {
    block_on(async {
        let (a, b, c) = (Owner::new(1), Owner::new(2), Owner::new(3));
        let mut m = Multisig::new("multisig", "valset");
        register(&mut m, &[&a, &b, &c], 2, &a).await;

        let to = Address::from([0x11u8; 20]);
        // two proposals: 100 wei and 999 wei, same nonce (a replacement)
        for value in [100u64, 999] {
            let mut cx = ctx(b"node", vec![]);
            exec(&mut m, &mut cx, propose_msg(&a, to, value, 0))
                .await
                .expect("propose");
        }

        // b signs the 100-wei transaction; submit it against the 999-wei one.
        let cheap = hash_for(to, 100, vec![], 0);
        let expensive = hash_for(to, 999, vec![], 0);
        let mut cx = ctx(b"node", vec![]);
        let err = exec(
            &mut m,
            &mut cx,
            MultisigMsg::Approve {
                vault_id: VAULT.into(),
                safe_tx_hash: expensive.to_vec(),
                signature: b.sign(cheap),
            },
        )
        .await
        .expect_err("a signature over one hash must not approve another");
        // it recovers to SOMEBODY, just not an owner — which is the whole point
        assert!(format!("{err:?}").contains("current owner"));

        assert!(executables(&m).await.is_empty());
    });
}

#[test]
fn proposal_nonce_must_be_within_the_chain_window() {
    block_on(async {
        let a = Owner::new(1);
        let mut m = Multisig::new("multisig", "valset");
        register(&mut m, &[&a], 1, &a).await;

        // advance the chain nonce to 5 (as the oracle would)
        let mut cx = ctx(b"validator", vec![b"validator".to_vec()]);
        exec(
            &mut m,
            &mut cx,
            MultisigMsg::RecordChainState {
                vault_id: VAULT.into(),
                nonce: 5,
                owners: vec![a.address().to_vec()],
                threshold: 1,
            },
        )
        .await
        .expect("record chain state");

        let to = Address::from([0x11u8; 20]);
        // below the chain nonce: can never execute
        let mut cx = ctx(b"node", vec![]);
        let err = exec(&mut m, &mut cx, propose_msg(&a, to, 1, 4))
            .await
            .expect_err("a spent nonce must be refused");
        assert!(format!("{err:?}").contains("below the chain nonce"));

        // far beyond the window: could only pin state
        let mut cx = ctx(b"node", vec![]);
        let err = exec(&mut m, &mut cx, propose_msg(&a, to, 1, 5 + 33))
            .await
            .expect_err("a nonce past the lookahead must be refused");
        assert!(format!("{err:?}").contains("past the chain nonce"));

        // at the chain nonce: fine
        let mut cx = ctx(b"node", vec![]);
        exec(&mut m, &mut cx, propose_msg(&a, to, 1, 5))
            .await
            .expect("the current nonce is proposable");
    });
}

/// The Safe is authoritative and the mirror is not. If the chain says the owner
/// set changed, we FREEZE rather than silently reconcile — an owner removed
/// on-chain must not keep approving here.
#[test]
fn owner_drift_freezes_the_vault() {
    block_on(async {
        let (a, b) = (Owner::new(1), Owner::new(2));
        let mut m = Multisig::new("multisig", "valset");
        register(&mut m, &[&a, &b], 1, &a).await;

        // the chain reports a DIFFERENT owner set (b was removed on-chain)
        let mut cx = ctx(b"validator", vec![b"validator".to_vec()]);
        exec(
            &mut m,
            &mut cx,
            MultisigMsg::RecordChainState {
                vault_id: VAULT.into(),
                nonce: 0,
                owners: vec![a.address().to_vec()],
                threshold: 1,
            },
        )
        .await
        .expect("record chain state");

        let to = Address::from([0x11u8; 20]);
        let mut cx = ctx(b"node", vec![]);
        let err = exec(&mut m, &mut cx, propose_msg(&a, to, 1, 0))
            .await
            .expect_err("a drifted vault must not accept proposals");
        assert!(format!("{err:?}").contains("disagrees with the chain"));
    });
}

/// Chain facts are unverifiable in consensus, so they are validator-gated. A
/// non-validator asserting them could freeze a vault or rewind its nonce.
#[test]
fn chain_facts_are_validator_gated() {
    block_on(async {
        let a = Owner::new(1);
        let mut m = Multisig::new("multisig", "valset");
        register(&mut m, &[&a], 1, &a).await;

        // "node" is not in the validator set
        let mut cx = ctx(b"node", vec![b"validator".to_vec()]);
        let err = exec(
            &mut m,
            &mut cx,
            MultisigMsg::RecordChainState {
                vault_id: VAULT.into(),
                nonce: 99,
                owners: vec![a.address().to_vec()],
                threshold: 1,
            },
        )
        .await
        .expect_err("a non-validator must not assert chain facts");
        assert!(format!("{err:?}").contains("validator"));

        let mut cx = ctx(b"node", vec![b"validator".to_vec()]);
        let err = exec(
            &mut m,
            &mut cx,
            MultisigMsg::RecordExecution {
                vault_id: VAULT.into(),
                safe_tx_hash: [0u8; 32].to_vec(),
                chain_tx_hash: [1u8; 32].to_vec(),
                success: true,
            },
        )
        .await
        .expect_err("a non-validator must not record an execution");
        assert!(format!("{err:?}").contains("validator"));
    });
}

#[test]
fn registration_requires_a_declared_owner_signature() {
    block_on(async {
        let (a, b) = (Owner::new(1), Owner::new(2));
        let stranger = Owner::new(9);
        let mut m = Multisig::new("multisig", "valset");

        let owner_bytes = vec![a.address().to_vec(), b.address().to_vec()];
        let safe = safe_addr().to_vec();
        let preimage = register_preimage(VAULT, CHAIN, &safe, &owner_bytes, 2);
        let mut cx = ctx(b"node", vec![]);
        let err = exec(
            &mut m,
            &mut cx,
            MultisigMsg::RegisterVault {
                vault_id: VAULT.into(),
                chain_id: CHAIN,
                safe_address: safe,
                owners: owner_bytes,
                threshold: 2,
                signature: stranger.sign_preimage(&preimage),
            },
        )
        .await
        .expect_err("a stranger must not register a vault");
        assert!(format!("{err:?}").contains("declared owner"));
    });
}

#[test]
fn threshold_must_be_within_the_owner_set() {
    block_on(async {
        let a = Owner::new(1);
        let mut m = Multisig::new("multisig", "valset");
        let owner_bytes = vec![a.address().to_vec()];
        let safe = safe_addr().to_vec();

        for threshold in [0u8, 2] {
            let preimage = register_preimage(VAULT, CHAIN, &safe, &owner_bytes, threshold);
            let mut cx = ctx(b"node", vec![]);
            let err = exec(
                &mut m,
                &mut cx,
                MultisigMsg::RegisterVault {
                    vault_id: VAULT.into(),
                    chain_id: CHAIN,
                    safe_address: safe.clone(),
                    owners: owner_bytes.clone(),
                    threshold,
                    signature: a.sign_preimage(&preimage),
                },
            )
            .await
            .expect_err("threshold must lie within 1..=owners");
            assert!(format!("{err:?}").contains("threshold"));
        }
    });
}

/// Attribution binding proves possession of the address; it must not accept a
/// proof minted for someone else's account.
#[test]
fn owner_binding_requires_possession_by_the_submitter() {
    block_on(async {
        let a = Owner::new(1);
        let mut m = Multisig::new("multisig", "valset");
        register(&mut m, &[&a], 1, &a).await;

        let addr = a.address().to_vec();
        // a proof bound to account "alice" replayed by account "mallory"
        let alice_preimage = bind_preimage(VAULT, &addr, b"alice");
        let mut cx = ctx(b"mallory", vec![]);
        let err = exec(
            &mut m,
            &mut cx,
            MultisigMsg::BindOwnerEoa {
                vault_id: VAULT.into(),
                address: addr.clone(),
                possession: a.sign_preimage(&alice_preimage),
            },
        )
        .await
        .expect_err("a binding proof must not replay onto another account");
        assert!(format!("{err:?}").contains("possession"));

        // the rightful account binds fine
        let mine = bind_preimage(VAULT, &addr, b"alice");
        let mut cx = ctx(b"alice", vec![]);
        exec(
            &mut m,
            &mut cx,
            MultisigMsg::BindOwnerEoa {
                vault_id: VAULT.into(),
                address: addr,
                possession: a.sign_preimage(&mine),
            },
        )
        .await
        .expect("the rightful owner binds");
    });
}

#[test]
fn snapshot_round_trips_and_install_verifies_the_root() {
    block_on(async {
        let (a, b, c) = (Owner::new(1), Owner::new(2), Owner::new(3));
        let mut m = Multisig::new("multisig", "valset");
        register(&mut m, &[&a, &b, &c], 2, &a).await;
        let to = Address::from([0x11u8; 20]);
        let mut cx = ctx(b"node", vec![]);
        exec(&mut m, &mut cx, propose_msg(&a, to, 100, 0))
            .await
            .expect("propose");

        let bytes = m.snapshot();
        let root = m.root();

        let mut fresh = Multisig::new("multisig", "valset");
        fresh.install(&bytes, root).expect("install");
        assert_eq!(fresh.root(), root);

        // a tampered snapshot is refused against the honest root
        let mut tampered = bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        let mut fresh = Multisig::new("multisig", "valset");
        assert!(fresh.install(&tampered, root).is_err());
    });
}
