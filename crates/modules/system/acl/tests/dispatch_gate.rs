//! the acl dispatch gate end-to-end through a REAL host: the kernel's drain
//! consults the acl module's policy before an `Origin::External` op reaches
//! its target and resolves the origin's principal against valset/identity.
//!
//! what these tests pin, in order:
//! - the DEFAULT is allow-all: with an empty table (and even with no acl
//!   module composed at all) an external op reaches its target module — the
//!   only refusals left are the target's own semantic gates.
//! - a set policy refuses a no-standing key at DISPATCH (the host's error,
//!   before the target module ever runs) and admits a key holding the
//!   required standing (the target module's own gate answers instead).
//! - clearing the entry restores the open default.
//! - module-origin follow-ups bypass policy (they are the host's machinery).
//!
//! ops are driven through `Host::submit_at` with `Origin::External(...)`,
//! exactly the shape the ordered lane hands the host after VERIFYING a frame
//! signature — so what these tests pin is the authorization model the live
//! network runs.

use acl::{Acl, AclMsg, Standing};
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use futures::executor::block_on;
use host::{BlockContext, Host, SubmitError};
use identity::{Identity, IdentityMsg, KeyScheme};
use sdk::{Error, Msg, Origin};
use sdk_testkit::MemStore;
use valset::{Valset, ValsetMsg};

const CHAIN: &str = "gate-chain";

fn keypair(seed: u64) -> PrivateKey {
    PrivateKey::from_seed(seed)
}

fn key_bytes(k: &PrivateKey) -> Vec<u8> {
    k.public_key().as_ref().to_vec()
}

/// a host with an EMPTY acl table, a valset seeded with member 1, and a bare
/// identity plane — the production system-module shape in miniature.
async fn gate_host() -> Host {
    let mut valset = Valset::new("valset", Box::new(MemStore::new()), "governance");
    valset
        .seed(key_bytes(&keypair(1)))
        .await
        .expect("seed valset");
    valset.finish_seed().await.expect("seed valset");
    Host::genesis(vec![
        Box::new(valset),
        Box::new(Acl::new("acl", Box::new(MemStore::new()), "governance")),
        Box::new(Identity::new(
            "identity",
            Box::new(MemStore::new()),
            CHAIN.into(),
        )),
    ])
    .expect("genesis")
}

async fn submit(
    host: &mut Host,
    origin: Origin,
    at: u64,
    target: &str,
    payload: Vec<u8>,
) -> Result<(), SubmitError> {
    host.submit_at(
        BlockContext {
            height: at,
            consensus_time: at,
            origin,
        },
        Msg {
            target: target.into(),
            payload,
        },
    )
    .await
    .map(|_| ())
}

/// set one acl entry as a governance-shaped module-origin follow-up.
async fn set_policy(host: &mut Host, at: u64, target: &str, standing: Option<Standing>) {
    submit(
        host,
        Origin::Module("governance".into()),
        at,
        "acl",
        acl::encode_msg(&AclMsg::SetPolicy {
            target: target.into(),
            standing,
        }),
    )
    .await
    .expect("policy write");
}

fn valset_grant(key: &PrivateKey) -> Vec<u8> {
    valset::encode_msg(&ValsetMsg::Grant {
        key: key_bytes(key),
    })
}

#[test]
fn the_default_is_allow_all_and_the_target_module_still_gates_semantically() {
    block_on(async {
        let mut host = gate_host().await;
        let nobody = keypair(9);

        // an EMPTY table admits any external origin to any target: the op
        // REACHES valset, whose own semantic gate produces the refusal — the
        // proof that dispatch let it through.
        let err = submit(
            &mut host,
            Origin::External(key_bytes(&nobody)),
            1,
            "valset",
            valset_grant(&nobody),
        )
        .await
        .expect_err("valset's own origin gate still refuses");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module { ref reason, .. })
                if reason == "not_governance"),
            "the refusal is the TARGET's, not the dispatch gate's: {err:?}"
        );
    });
}

#[test]
fn a_set_policy_refuses_no_standing_keys_at_dispatch_and_clears_back_to_open() {
    block_on(async {
        let mut host = gate_host().await;
        let (member, nobody) = (keypair(1), keypair(9));

        set_policy(&mut host, 1, "acl", Some(Standing::Validator)).await;

        // a no-standing key is refused by the DISPATCH gate — the acl module's
        // own "only via governance" never gets a chance to answer.
        let probe = acl::encode_msg(&AclMsg::SetPolicy {
            target: "chat".into(),
            standing: None,
        });
        let err = submit(
            &mut host,
            Origin::External(key_bytes(&nobody)),
            2,
            "acl",
            probe.clone(),
        )
        .await
        .expect_err("no validator standing");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module { ref reason, ref sentence })
                if reason == "acl_standing"
                    && sentence.contains("acl: target acl requires validator standing")),
            "the refusal is the dispatch gate's: {err:?}"
        );

        // the seeded VALIDATOR passes the dispatch gate — and then hits the acl
        // module's own semantic origin gate, proving the op reached the module.
        let err = submit(
            &mut host,
            Origin::External(key_bytes(&member)),
            3,
            "acl",
            probe.clone(),
        )
        .await
        .expect_err("acl's own gate still refuses external writes");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module { ref reason, .. })
                if reason == "not_governance"),
            "got {err:?}"
        );

        // clearing the entry restores the open default for everyone.
        set_policy(&mut host, 4, "acl", None).await;
        let err = submit(
            &mut host,
            Origin::External(key_bytes(&nobody)),
            5,
            "acl",
            probe,
        )
        .await
        .expect_err("back to the module's own gate");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module { ref reason, .. })
                if reason == "not_governance"),
            "the dispatch gate is open again: {err:?}"
        );
    });
}

#[test]
fn node_standing_admits_residents_and_the_wildcard_covers_unlisted_targets() {
    block_on(async {
        let mut host = gate_host().await;
        let (member, resident, nobody) = (keypair(1), keypair(2), keypair(9));

        // grant resident standing (a module-origin write — the gate bypasses
        // policy for the host's own machinery even after the "*" entry below).
        submit(
            &mut host,
            Origin::Module("governance".into()),
            1,
            "valset",
            valset_grant(&resident),
        )
        .await
        .expect("resident grant");

        set_policy(&mut host, 2, "*", Some(Standing::Node)).await;

        // the wildcard covers a target with no exact entry: valset itself.
        let probe = valset_grant(&nobody);
        let err = submit(
            &mut host,
            Origin::External(key_bytes(&nobody)),
            3,
            "valset",
            probe.clone(),
        )
        .await
        .expect_err("no node standing");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module { ref reason, ref sentence })
                if reason == "acl_standing" && sentence.contains("requires node standing")),
            "got {err:?}"
        );

        // a resident AND a validator both hold node standing: the op passes
        // dispatch and valset's own gate answers.
        for holder in [&resident, &member] {
            let err = submit(
                &mut host,
                Origin::External(key_bytes(holder)),
                4,
                "valset",
                probe.clone(),
            )
            .await
            .expect_err("valset's own gate answers");
            assert!(
                matches!(err, SubmitError::Rejected(Error::Module { ref reason, .. })
                    if reason == "not_governance"),
                "got {err:?}"
            );
        }
    });
}

#[test]
fn user_standing_resolves_through_the_identity_account_plane() {
    block_on(async {
        let mut host = gate_host().await;
        let (founder, nobody) = (keypair(10), keypair(9));
        let node_key = key_bytes(&keypair(1)); // a valset member — still no account

        // found an account for the founder's key.
        submit(
            &mut host,
            Origin::External(key_bytes(&founder)),
            1,
            "identity",
            identity::encode_msg(&IdentityMsg::Create {
                name: "founder".into(),
                scheme: KeyScheme::Ed25519,
            }),
        )
        .await
        .expect("account founded");

        set_policy(&mut host, 2, "identity", Some(Standing::User)).await;

        // the founder's key resolves to the account — the op passes dispatch
        // (identity then answers itself).
        let probe = identity::encode_msg(&IdentityMsg::SetName {
            name: "gate".into(),
        });
        submit(
            &mut host,
            Origin::External(key_bytes(&founder)),
            3,
            "identity",
            probe.clone(),
        )
        .await
        .expect("an account key passes user standing");

        // a NODE key holds node standing, never user standing: no account
        // is ever keyed by a node.
        let err = submit(
            &mut host,
            Origin::External(node_key),
            4,
            "identity",
            probe.clone(),
        )
        .await
        .expect_err("a node key is not a user");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module { ref reason, ref sentence })
                if reason == "acl_standing" && sentence.contains("requires user standing")),
            "got {err:?}"
        );

        // an account-less key is refused at dispatch.
        let err = submit(
            &mut host,
            Origin::External(key_bytes(&nobody)),
            5,
            "identity",
            probe,
        )
        .await
        .expect_err("no account");
        assert!(
            matches!(err, SubmitError::Rejected(Error::Module { ref reason, ref sentence })
                if reason == "acl_standing" && sentence.contains("requires user standing")),
            "got {err:?}"
        );
    });
}
