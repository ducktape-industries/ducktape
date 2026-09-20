//! state-sync round-trip: a joiner reconstructs a byte-identical qmdb root by
//! pulling a source store's operation range through commonware's qmdb sync,
//! then wraps a fresh `Governance` around the injected store — the same
//! discriminating property chat, pages, agent, and automations prove, over
//! the proposal + redemption + roster layout.
//!
//! the source SEEDS the `__config` invite-binding record exactly the way the
//! production genesis path does (`bin/node/src/host_state.rs`
//! `seed_store_config`), then settles one proposal through the full
//! propose/vote/execute path (record overwrites), leaves a second proposal
//! Open, and redeems one invite (a redemption point record + an emitted
//! grant), so the op log carries overwrites, not just inserts. only a real
//! sync that ships the ACTUAL proven op range lands on the same root — and
//! the config record arrives with it, which is what lets a joiner's wasm
//! guest read its invite binding from the synced store.

use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use governance::invite::{INVITE_GRANT_NAMESPACE, INVITE_NONCE_LEN, InviteToken};
use governance::{
    GovAction, GovMsg, GovQuery, GovReply, Governance, decode_reply, encode_msg, encode_query,
    invite::sign_join_proof,
};
use sdk::{Env, MerkleStore as _, Module, Msg, Origin, StateRoot};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;
use valset::{
    ValsetQuery, ValsetReply, decode_query as valset_decode_query,
    encode_reply as valset_encode_reply,
};

const BINDING: &[u8] = b"sync-net#feedface";

fn keypair(seed: u8) -> PrivateKey {
    let seed = [seed; 32];
    PrivateKey::decode(&seed[..]).expect("any 32 bytes is a valid seed")
}

fn key_bytes(k: &PrivateKey) -> Vec<u8> {
    k.public_key().as_ref().to_vec()
}

/// a ctx whose valset sibling answers with `member` as the sole validator and
/// an empty resident set — the reads Propose/Vote/Redeem consume.
fn ctx(height: u64, origin: Origin, member: Vec<u8>) -> TestCtx {
    TestCtx::with_env(Env {
        height,
        consensus_time: height,
        origin,
        me: "governance".into(),
        cause: sdk::Cause::Direct,
    })
    .on_query("valset", move |req| {
        match valset_decode_query(req).expect("valset query decodes") {
            ValsetQuery::Validators => Ok(valset_encode_reply(&ValsetReply::Validators(vec![
                member.clone(),
            ]))),
            ValsetQuery::Residents => Ok(valset_encode_reply(&ValsetReply::Residents(Vec::new()))),
            ValsetQuery::MeshWindow => {
                Ok(valset_encode_reply(&ValsetReply::MeshWindow(Vec::new())))
            }
        }
    })
}

fn gov(m: &GovMsg) -> Msg {
    Msg {
        target: "governance".into(),
        payload: encode_msg(m),
    }
}

/// mint a bearer token as `issuer` (fixed nonce — tests need determinism) and
/// wrap it, with `joiner`'s proof-of-possession, into the Redeem op.
fn redeem(issuer: &PrivateKey, nonce_byte: u8, joiner: &PrivateKey) -> Msg {
    let nonce = [nonce_byte; INVITE_NONCE_LEN];
    let expires: u64 = u64::MAX;
    let preimage = [BINDING, nonce.as_slice(), &expires.to_le_bytes()].concat();
    let token = InviteToken {
        issuer: issuer.public_key(),
        nonce,
        expires_unix_secs: expires,
        sig: issuer.sign(INVITE_GRANT_NAMESPACE, &preimage),
    };
    let proof = sign_join_proof(joiner, BINDING, &token);
    gov(&GovMsg::Redeem {
        issuer: key_bytes(issuer),
        nonce: nonce.to_vec(),
        token_sig: token.sig.encode().as_ref().to_vec(),
        joiner: key_bytes(joiner),
        proof: proof.encode().as_ref().to_vec(),
        expires_unix_secs: expires,
    })
}

// drive one op through the REAL module path: execute + commit_block (one op
// per block-height), so the committed op log is what a validator produces.
async fn apply_commit(m: &mut Governance, height: u64, member: &PrivateKey, op: Msg) {
    let mut c = ctx(
        height,
        Origin::External(key_bytes(member)),
        key_bytes(member),
    );
    m.execute(&mut c, &op).await.unwrap();
    m.commit_block().await.unwrap();
}

/// the read matrix compared source-vs-joiner: the roster-served listing, the
/// open proposal's point read, the redemption point read, and the shares view.
const QUERIES: [&str; 4] = ["proposals", "upgrade", "redemption", "shares"];

async fn replies(m: &Governance) -> Vec<GovReply> {
    let queries = [
        GovQuery::Proposals,
        GovQuery::Proposal {
            proposal_id: "upgrade".into(),
        },
        GovQuery::Redemption {
            nonce: vec![7; INVITE_NONCE_LEN],
        },
        GovQuery::Shares,
    ];
    let mut out = Vec::new();
    for q in &queries {
        out.push(decode_reply(&m.query(&encode_query(q)).await.unwrap()).unwrap());
    }
    out
}

fn governance_over(store: Box<dyn sdk::MerkleStore>) -> Governance {
    Governance::new("governance", store, "valset", "identity")
        .with_invite_binding(BINDING)
        .with_code_registry("modules")
}

#[test]
fn synced_store_reconstructs_source_root_proposals_and_redemptions() {
    deterministic::Runner::default().start(|context| async move {
        let member = keypair(1);
        let joiner = keypair(9);

        // SOURCE: seed the genesis-config record the way the production
        // genesis path does, THEN wrap the module — the config is committed
        // store state under the shared `sdk::store_key` convention, part of
        // the root from block zero.
        let mut src_store = QmdbStore::init(context.child("src"), "src").await;
        let config = sdk::genesis_config::encode_config(&[("invite", BINDING)]);
        src_store
            .commit_batch(vec![(
                sdk::store_key(sdk::genesis_config::CONFIG_KEY),
                Some(config.clone()),
            )])
            .await
            .expect("seed genesis config");
        let config_root = src_store.root();
        assert_ne!(config_root, StateRoot::ZERO, "config alone moves the root");
        let mut src = governance_over(Box::new(src_store));

        // settle one proposal end to end (insert + two overwrites): a sole
        // member's yes-ballot is early-decidable at majority 1.
        apply_commit(
            &mut src,
            1,
            &member,
            gov(&GovMsg::Propose {
                proposal_id: "signal".into(),
                action: GovAction::Signal {
                    text: "sync me".into(),
                },
                voting_period: 50,
            }),
        )
        .await;
        apply_commit(
            &mut src,
            2,
            &member,
            gov(&GovMsg::Vote {
                proposal_id: "signal".into(),
                approve: true,
            }),
        )
        .await;
        apply_commit(
            &mut src,
            3,
            &member,
            gov(&GovMsg::Execute {
                proposal_id: "signal".into(),
            }),
        )
        .await;
        // a second proposal stays Open — roster overwrite + fresh record.
        apply_commit(
            &mut src,
            4,
            &member,
            gov(&GovMsg::Propose {
                proposal_id: "upgrade".into(),
                action: GovAction::UpdateModule {
                    name: "hello-replacement".into(),
                    module_id: "hello".into(),
                    activation_lead: 900,
                    code_hash: vec![3; 32],
                },
                voting_period: 100,
            }),
        )
        .await;
        // one redeemed invite: the exactly-once point record.
        apply_commit(&mut src, 5, &member, redeem(&member, 7, &joiner)).await;

        let src_root: StateRoot = src.root();
        assert_ne!(src_root, config_root, "the ops moved the root");
        let src_replies = replies(&src).await;
        let GovReply::Proposals(views) = &src_replies[0] else {
            panic!("expected the listing");
        };
        assert_eq!(
            views.len(),
            1,
            "the settled proposal left the roster; only the open one remains"
        );

        // the module consumed its store, so REOPEN the committed partitions
        // as a bare store for the handoff (drop first — one owner at a time).
        drop(src);
        let src_store = QmdbStore::init(context.child("src_serve"), "src").await;
        assert_eq!(
            src_store.root(),
            src_root,
            "reopened store must recover the committed root"
        );

        // describe the target (root + op range), THEN hand the source off as
        // the sync resolver (consumes it — order matters).
        let target = src_store.sync_boundary_target().await;
        let resolver = src_store.into_resolver();

        // JOINER: reconstruct on a FRESH context + namespace by pulling from
        // the resolver, then wrap the module around the injected store — the
        // exact shape a joining host uses. no ops are applied in application
        // order on this side.
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");

        // the genesis-config record ARRIVED with the op range: this is what a
        // joiner's wasm guest reads its invite binding from.
        assert_eq!(
            store
                .get(&sdk::store_key(sdk::genesis_config::CONFIG_KEY))
                .await
                .expect("config read"),
            Some(config),
            "the __config record rides the sync"
        );
        let synced = governance_over(Box::new(store));

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // roster, proposal records, and the redemption record synced together:
        // the joiner answers every read exactly like the source.
        let synced_replies = replies(&synced).await;
        for (name, (a, b)) in QUERIES.iter().zip(src_replies.iter().zip(&synced_replies)) {
            assert_eq!(a, b, "the {name} reply diverged");
        }
        let GovReply::Redemption(Some(redemption)) = &synced_replies[2] else {
            panic!("the redemption record must be present on the joiner");
        };
        assert_eq!(redemption.joiner, key_bytes(&joiner));
        assert_eq!(redemption.height, 5);
    });
}
