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
use sdk::{Error, Module, ModuleId, Msg, Origin, StateRoot};
use sdk_testkit::MemStore;

fn keypair(seed: u64) -> PrivateKey {
    PrivateKey::from_seed(seed)
}

fn key_bytes(k: &PrivateKey) -> Vec<u8> {
    k.public_key().as_ref().to_vec()
}

/// The gate's production reads are byte contracts, not implementation
/// contracts. Keep this test's doubles local so the ACL producer never links
/// the valset or identity modules just to exercise host dispatch.
mod sibling_contracts {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ValsetMsg {
        Join { key: Vec<u8> },
        Leave { key: Vec<u8> },
        Grant { key: Vec<u8> },
        Revoke { key: Vec<u8> },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ValsetQuery {
        Validators,
        Residents,
        MeshWindow,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ValsetReply {
        Validators(Vec<Vec<u8>>),
        Residents(Vec<Vec<u8>>),
        MeshWindow(Vec<GenerationSet>),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct GenerationSet {
        pub generation: u64,
        pub validators: Vec<Vec<u8>>,
        pub residents: Vec<Vec<u8>>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum IdentityMsg {
        Create { name: String, scheme: String },
        SetName { name: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum IdentityQuery {
        OfKey { key: Vec<u8> },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum IdentityReply {
        Account(Option<AccountView>),
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct AccountView {
        pub number: u64,
        pub name: String,
        pub control: Control,
        pub keys: Vec<KeyView>,
        pub avatar: Option<String>,
        pub bio: Option<String>,
        pub updated_at: u64,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum Control {
        Keys,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct KeyView {
        pub scheme: String,
        pub pubkey: Vec<u8>,
        pub label: Option<String>,
        pub added_at: u64,
    }

    pub fn encode_valset_msg(msg: &ValsetMsg) -> Vec<u8> {
        sdk::wire::encode(msg)
    }

    pub fn decode_valset_msg(bytes: &[u8]) -> Result<ValsetMsg, String> {
        sdk::wire::decode(bytes)
    }

    pub fn decode_valset_query(bytes: &[u8]) -> Result<ValsetQuery, String> {
        sdk::wire::decode(bytes)
    }

    pub fn encode_valset_reply(reply: &ValsetReply) -> Vec<u8> {
        sdk::wire::encode(reply)
    }

    pub fn decode_identity_msg(bytes: &[u8]) -> Result<IdentityMsg, String> {
        sdk::wire::decode(bytes)
    }

    pub fn encode_identity_msg(msg: &IdentityMsg) -> Vec<u8> {
        sdk::wire::encode(msg)
    }

    pub fn decode_identity_query(bytes: &[u8]) -> Result<IdentityQuery, String> {
        sdk::wire::decode(bytes)
    }

    pub fn encode_identity_reply(reply: &IdentityReply) -> Vec<u8> {
        sdk::wire::encode(reply)
    }
}

type Tiers = (Vec<Vec<u8>>, Vec<Vec<u8>>);

struct ValsetStub {
    validators: Vec<Vec<u8>>,
    residents: Vec<Vec<u8>>,
    staged: Option<Tiers>,
}

impl ValsetStub {
    fn new(validator: Vec<u8>) -> Self {
        Self {
            validators: vec![validator],
            residents: Vec::new(),
            staged: None,
        }
    }

    fn view(&self) -> Tiers {
        self.staged
            .clone()
            .unwrap_or_else(|| (self.validators.clone(), self.residents.clone()))
    }
}

#[async_trait::async_trait(?Send)]
impl Module for ValsetStub {
    fn id(&self) -> ModuleId {
        "valset".into()
    }

    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }

    async fn execute(&mut self, ctx: &mut dyn sdk::Ctx, msg: &Msg) -> Result<(), Error> {
        let governance_origin =
            matches!(&ctx.env().origin, Origin::Module(id) if id == "governance");
        if !governance_origin {
            return Err(Error::module(
                "not_governance",
                "valset: membership changes only via governance",
            ));
        }
        let command = sibling_contracts::decode_valset_msg(&msg.payload)
            .map_err(|e| Error::module("codec", e))?;
        let (mut validators, mut residents) = self.view();
        match command {
            sibling_contracts::ValsetMsg::Join { key } => {
                if !validators.contains(&key) {
                    validators.push(key);
                }
            }
            sibling_contracts::ValsetMsg::Leave { key } => {
                validators.retain(|member| member != &key);
            }
            sibling_contracts::ValsetMsg::Grant { key } => {
                if !residents.contains(&key) {
                    residents.push(key);
                }
            }
            sibling_contracts::ValsetMsg::Revoke { key } => {
                residents.retain(|member| member != &key);
            }
        }
        self.staged = Some((validators, residents));
        Ok(())
    }

    async fn query(&self, request: &[u8]) -> Result<Vec<u8>, Error> {
        let query = sibling_contracts::decode_valset_query(request)
            .map_err(|e| Error::module("codec", e))?;
        let (validators, residents) = self.view();
        let reply = match query {
            sibling_contracts::ValsetQuery::Validators => {
                sibling_contracts::ValsetReply::Validators(validators)
            }
            sibling_contracts::ValsetQuery::Residents => {
                sibling_contracts::ValsetReply::Residents(residents)
            }
            sibling_contracts::ValsetQuery::MeshWindow => {
                sibling_contracts::ValsetReply::MeshWindow(Vec::new())
            }
        };
        Ok(sibling_contracts::encode_valset_reply(&reply))
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some((validators, residents)) = self.staged.take() {
            self.validators = validators;
            self.residents = residents;
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

struct IdentityStub {
    keys: Vec<Vec<u8>>,
    staged: Option<Vec<Vec<u8>>>,
}

impl IdentityStub {
    fn new() -> Self {
        Self {
            keys: Vec::new(),
            staged: None,
        }
    }

    fn view(&self) -> Vec<Vec<u8>> {
        self.staged.clone().unwrap_or_else(|| self.keys.clone())
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

    async fn execute(&mut self, ctx: &mut dyn sdk::Ctx, msg: &Msg) -> Result<(), Error> {
        let Origin::External(key) = &ctx.env().origin else {
            return Err(Error::module(
                "not_external",
                "identity: expected an external key",
            ));
        };
        let command = sibling_contracts::decode_identity_msg(&msg.payload)
            .map_err(|e| Error::module("codec", e))?;
        let mut keys = self.view();
        match command {
            sibling_contracts::IdentityMsg::Create { .. } => {
                if keys.contains(key) {
                    return Err(Error::module(
                        "key_already_claimed",
                        "identity: key already belongs to an account",
                    ));
                }
                keys.push(key.clone());
            }
            sibling_contracts::IdentityMsg::SetName { .. } => {
                if !keys.contains(key) {
                    return Err(Error::module(
                        "no_identity_account",
                        "identity: origin key belongs to no account",
                    ));
                }
            }
        }
        self.staged = Some(keys);
        Ok(())
    }

    async fn query(&self, request: &[u8]) -> Result<Vec<u8>, Error> {
        let sibling_contracts::IdentityQuery::OfKey { key } =
            sibling_contracts::decode_identity_query(request)
                .map_err(|e| Error::module("codec", e))?;
        let account = self
            .view()
            .contains(&key)
            .then(|| sibling_contracts::AccountView {
                number: 1,
                name: "founder".into(),
                control: sibling_contracts::Control::Keys,
                keys: vec![sibling_contracts::KeyView {
                    scheme: "ed25519".into(),
                    pubkey: key,
                    label: None,
                    added_at: 1,
                }],
                avatar: None,
                bio: None,
                updated_at: 1,
            });
        Ok(sibling_contracts::encode_identity_reply(
            &sibling_contracts::IdentityReply::Account(account),
        ))
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(keys) = self.staged.take() {
            self.keys = keys;
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

/// a host with an EMPTY acl table, a valset seeded with member 1, and a bare
/// identity plane — the production system-module shape in miniature.
fn gate_host() -> Host {
    Host::genesis(vec![
        Box::new(ValsetStub::new(key_bytes(&keypair(1)))),
        Box::new(Acl::new("acl", Box::new(MemStore::new()), "governance")),
        Box::new(IdentityStub::new()),
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
    sibling_contracts::encode_valset_msg(&sibling_contracts::ValsetMsg::Grant {
        key: key_bytes(key),
    })
}

#[test]
fn the_default_is_allow_all_and_the_target_module_still_gates_semantically() {
    block_on(async {
        let mut host = gate_host();
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
        let mut host = gate_host();
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
        let mut host = gate_host();
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
        let mut host = gate_host();
        let (founder, nobody) = (keypair(10), keypair(9));
        let node_key = key_bytes(&keypair(1)); // a valset member — still no account

        // found an account for the founder's key.
        submit(
            &mut host,
            Origin::External(key_bytes(&founder)),
            1,
            "identity",
            sibling_contracts::encode_identity_msg(&sibling_contracts::IdentityMsg::Create {
                name: "founder".into(),
                scheme: "ed25519".into(),
            }),
        )
        .await
        .expect("account founded");

        set_policy(&mut host, 2, "identity", Some(Standing::User)).await;

        // the founder's key resolves to the account — the op passes dispatch
        // (identity then answers itself).
        let probe =
            sibling_contracts::encode_identity_msg(&sibling_contracts::IdentityMsg::SetName {
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
