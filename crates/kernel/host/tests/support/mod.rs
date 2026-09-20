//! Owned test doubles for governance's runtime contracts.
//!
//! These doubles intentionally implement only the sibling behavior governance
//! consumes. Keeping them here makes the governance producer and its tests
//! compile without linking another system-module implementation.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use governance::identity_contract::{self, AccountView, IdentityQuery, IdentityReply};
use module_artifact::LaneDecl;
use sdk::{Ctx, Error, Module, ModuleId, Msg, Origin, StateRoot, StateSyncHandle};
use serde::{Deserialize, Serialize};

fn wire<T: Serialize>(value: &T) -> Vec<u8> {
    sdk::wire::encode(value)
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, String> {
    sdk::wire::decode(bytes)
}

fn governance_origin(ctx: &dyn Ctx, id: &str) -> Result<(), Error> {
    match &ctx.env().origin {
        Origin::System => Ok(()),
        Origin::Module(origin) if origin == id => Ok(()),
        origin => Err(Error::module(
            "not_governance",
            format!("test registry requires System or Module({id:?}), got {origin:?}"),
        )),
    }
}

pub mod valset {
    use super::*;

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
    #[serde(deny_unknown_fields)]
    pub struct GenerationSet {
        pub generation: u64,
        pub validators: Vec<Vec<u8>>,
        pub residents: Vec<Vec<u8>>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ValsetReply {
        Validators(Vec<Vec<u8>>),
        Residents(Vec<Vec<u8>>),
        MeshWindow(Vec<GenerationSet>),
    }

    pub fn encode_msg(msg: &ValsetMsg) -> Vec<u8> {
        wire(msg)
    }

    pub fn decode_msg(bytes: &[u8]) -> Result<ValsetMsg, String> {
        decode(bytes)
    }

    pub fn encode_query(query: &ValsetQuery) -> Vec<u8> {
        wire(query)
    }

    pub fn decode_query(bytes: &[u8]) -> Result<ValsetQuery, String> {
        decode(bytes)
    }

    pub fn encode_reply(reply: &ValsetReply) -> Vec<u8> {
        wire(reply)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<ValsetReply, String> {
        decode(bytes)
    }

    pub struct Valset {
        id: ModuleId,
        governance_id: ModuleId,
        validators: BTreeSet<Vec<u8>>,
        residents: BTreeSet<Vec<u8>>,
    }

    impl Valset {
        pub fn new(
            id: impl Into<ModuleId>,
            _store: Box<dyn sdk::MerkleStore>,
            governance_id: impl Into<ModuleId>,
        ) -> Self {
            Self {
                id: id.into(),
                governance_id: governance_id.into(),
                validators: BTreeSet::new(),
                residents: BTreeSet::new(),
            }
        }

        pub async fn seed(&mut self, key: Vec<u8>) -> Result<(), Error> {
            self.validators.insert(key);
            Ok(())
        }

        pub async fn finish_seed(&mut self) -> Result<(), Error> {
            Ok(())
        }

        fn members(&self) -> Vec<Vec<u8>> {
            self.validators.iter().cloned().collect()
        }

        fn resident_list(&self) -> Vec<Vec<u8>> {
            self.residents.iter().cloned().collect()
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Module for Valset {
        fn id(&self) -> ModuleId {
            self.id.clone()
        }

        fn root(&self) -> StateRoot {
            StateRoot::ZERO
        }

        fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
            Ok(StateSyncHandle::Stateless)
        }

        async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
            governance_origin(ctx, &self.governance_id)?;
            match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
                ValsetMsg::Join { key } => {
                    self.residents.remove(&key);
                    self.validators.insert(key);
                    Ok(())
                }
                ValsetMsg::Leave { key } => {
                    let removing_last =
                        self.validators.len() == 1 && self.validators.contains(&key);
                    if removing_last {
                        return Err(Error::module(
                            "last_validator",
                            "cannot empty validator set",
                        ));
                    }
                    self.validators.remove(&key);
                    Ok(())
                }
                ValsetMsg::Grant { key } => {
                    if !self.validators.contains(&key) {
                        self.residents.insert(key);
                    }
                    Ok(())
                }
                ValsetMsg::Revoke { key } => {
                    self.residents.remove(&key);
                    Ok(())
                }
            }
        }

        async fn query(&self, request: &[u8]) -> Result<Vec<u8>, Error> {
            match decode_query(request).map_err(|e| Error::module("codec", e))? {
                ValsetQuery::Validators => {
                    Ok(encode_reply(&ValsetReply::Validators(self.members())))
                }
                ValsetQuery::Residents => {
                    Ok(encode_reply(&ValsetReply::Residents(self.resident_list())))
                }
                ValsetQuery::MeshWindow => Ok(encode_reply(&ValsetReply::MeshWindow(Vec::new()))),
            }
        }
    }
}

pub mod identity {
    use super::*;

    pub struct Identity {
        id: ModuleId,
        accounts: BTreeMap<u64, AccountView>,
        by_key: BTreeMap<Vec<u8>, u64>,
    }

    impl Identity {
        pub fn new(
            id: impl Into<ModuleId>,
            _store: Box<dyn sdk::MerkleStore>,
            _chain_id: String,
        ) -> Self {
            Self {
                id: id.into(),
                accounts: BTreeMap::new(),
                by_key: BTreeMap::new(),
            }
        }

        pub fn from_accounts(entries: Vec<(u64, Vec<Vec<u8>>)>) -> Self {
            let mut accounts = BTreeMap::new();
            let mut by_key = BTreeMap::new();
            for (number, keys) in entries {
                for key in &keys {
                    by_key.insert(key.clone(), number);
                }
                accounts.insert(
                    number,
                    AccountView {
                        number,
                        name: format!("account-{number}"),
                        control: identity_contract::Control::Keys,
                        keys: keys
                            .into_iter()
                            .map(|pubkey| identity_contract::KeyView {
                                scheme: identity_contract::KeyScheme::Ed25519,
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
            Self {
                id: "identity".into(),
                accounts,
                by_key,
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Module for Identity {
        fn id(&self) -> ModuleId {
            self.id.clone()
        }

        fn root(&self) -> StateRoot {
            StateRoot::ZERO
        }

        fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
            Ok(StateSyncHandle::Stateless)
        }

        async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
            Err(Error::module(
                "read_only",
                "identity test stub is read-only",
            ))
        }

        async fn query(&self, request: &[u8]) -> Result<Vec<u8>, Error> {
            let reply = match identity_contract::decode_query(request)
                .map_err(|e| Error::module("codec", e))?
            {
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
                IdentityQuery::Resolve { references } => IdentityReply::Resolved(
                    references
                        .into_iter()
                        .map(|reference| match reference {
                            identity_contract::AccountRef::Account(number) => {
                                self.accounts.contains_key(&number).then_some(number)
                            }
                            identity_contract::AccountRef::Key(key) => {
                                self.by_key.get(&key).copied()
                            }
                        })
                        .collect(),
                ),
                IdentityQuery::KeyGen { key } => {
                    IdentityReply::Gen(u64::from(self.by_key.contains_key(&key)))
                }
                IdentityQuery::Controlled { .. } => IdentityReply::Accounts(Vec::new()),
            };
            Ok(identity_contract::encode_reply(&reply))
        }
    }
}

pub mod acl {
    use super::*;
    pub use governance::Standing;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum AclMsg {
        SetPolicy {
            target: String,
            standing: Option<Standing>,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum AclQuery {
        Policy,
        PolicyFor { target: String },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum AclReply {
        Policy(Vec<(String, Standing)>),
        PolicyFor(Option<Standing>),
    }

    pub fn encode_msg(msg: &AclMsg) -> Vec<u8> {
        wire(msg)
    }

    pub fn decode_msg(bytes: &[u8]) -> Result<AclMsg, String> {
        decode(bytes)
    }

    pub fn encode_query(query: &AclQuery) -> Vec<u8> {
        wire(query)
    }

    pub fn decode_query(bytes: &[u8]) -> Result<AclQuery, String> {
        decode(bytes)
    }

    pub fn encode_reply(reply: &AclReply) -> Vec<u8> {
        wire(reply)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<AclReply, String> {
        decode(bytes)
    }

    pub struct Acl {
        id: ModuleId,
        governance_id: ModuleId,
        policies: BTreeMap<String, Standing>,
    }

    impl Acl {
        pub fn new(
            id: impl Into<ModuleId>,
            _store: Box<dyn sdk::MerkleStore>,
            governance_id: impl Into<ModuleId>,
        ) -> Self {
            Self {
                id: id.into(),
                governance_id: governance_id.into(),
                policies: BTreeMap::new(),
            }
        }

        fn effective(&self, target: &str) -> Option<Standing> {
            self.policies
                .get(target)
                .cloned()
                .or_else(|| self.policies.get("*").cloned())
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Module for Acl {
        fn id(&self) -> ModuleId {
            self.id.clone()
        }

        fn root(&self) -> StateRoot {
            StateRoot::ZERO
        }

        fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
            Ok(StateSyncHandle::Stateless)
        }

        async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
            governance_origin(ctx, &self.governance_id)?;
            let AclMsg::SetPolicy { target, standing } =
                decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))?;
            let valid_target = !target.is_empty() && target.trim() == target && target.len() <= 64;
            if !valid_target {
                return Err(Error::module("bad_acl_target", "invalid acl target"));
            }
            match standing {
                Some(standing) => {
                    self.policies.insert(target, standing);
                }
                None => {
                    self.policies.remove(&target);
                }
            }
            Ok(())
        }

        async fn query(&self, request: &[u8]) -> Result<Vec<u8>, Error> {
            let reply = match decode_query(request).map_err(|e| Error::module("codec", e))? {
                AclQuery::Policy => AclReply::Policy(
                    self.policies
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ),
                AclQuery::PolicyFor { target } => AclReply::PolicyFor(self.effective(&target)),
            };
            Ok(encode_reply(&reply))
        }
    }
}

pub mod modules {
    use super::*;

    pub const CODE_HASH_LEN: usize = 32;
    pub const MIN_SWAP_LEAD: u64 = 3;
    pub use governance::Kind;

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesMsg {
        RegisterModule {
            module_id: String,
            kind: Kind,
            code_hash: Vec<u8>,
            lanes: Vec<LaneDecl>,
        },
        ScheduleSwap {
            name: String,
            module_id: String,
            activation_height: u64,
            code_hash: Vec<u8>,
        },
        ScheduleRegister {
            name: String,
            module_id: String,
            kind: Kind,
            activation_height: u64,
            code_hash: Vec<u8>,
            lanes: Vec<LaneDecl>,
        },
        CancelSwap {
            name: String,
            module_id: String,
        },
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesQuery {
        ModuleStatus,
        ArmedAt { height: u64 },
        Lanes,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ScheduledSwap {
        pub name: String,
        pub activation_height: u64,
        pub code_hash: Vec<u8>,
        pub readiness: Vec<Vec<u8>>,
        pub ready_at: Option<u64>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Activation {
        pub height: u64,
        pub code_hash: Vec<u8>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ModuleCode {
        pub module_id: String,
        pub kind: Kind,
        pub active_code_hash: Vec<u8>,
        pub pending: Option<ScheduledSwap>,
        pub history: Vec<Activation>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ArmedSwap {
        pub module_id: String,
        pub code_hash: Vec<u8>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct LaneRecord {
        pub id: u8,
        pub module_id: String,
        pub name: String,
        pub stream: Option<module_artifact::LaneStream>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    pub enum ModulesReply {
        ModuleStatus { modules: Vec<ModuleCode> },
        ArmedAt { swaps: Vec<ArmedSwap> },
        Lanes { lanes: Vec<LaneRecord> },
    }

    pub fn encode_msg(msg: &ModulesMsg) -> Vec<u8> {
        wire(msg)
    }

    pub fn decode_msg(bytes: &[u8]) -> Result<ModulesMsg, String> {
        decode(bytes)
    }

    pub fn encode_query(query: &ModulesQuery) -> Vec<u8> {
        wire(query)
    }

    pub fn decode_query(bytes: &[u8]) -> Result<ModulesQuery, String> {
        decode(bytes)
    }

    pub fn encode_reply(reply: &ModulesReply) -> Vec<u8> {
        wire(reply)
    }

    pub fn decode_reply(bytes: &[u8]) -> Result<ModulesReply, String> {
        decode(bytes)
    }

    pub struct Modules {
        id: ModuleId,
        entries: BTreeMap<String, ModuleCode>,
    }

    impl Modules {
        pub fn new(
            id: impl Into<ModuleId>,
            _store: Box<dyn sdk::MerkleStore>,
            _valset_id: impl Into<ModuleId>,
            _governance_id: impl Into<ModuleId>,
        ) -> Self {
            Self {
                id: id.into(),
                entries: BTreeMap::new(),
            }
        }

        fn authorize(ctx: &dyn Ctx) -> Result<(), Error> {
            let is_system = matches!(ctx.env().origin, Origin::System);
            let is_governance =
                matches!(&ctx.env().origin, Origin::Module(id) if id == "governance");
            if is_system || is_governance {
                return Ok(());
            }
            Err(Error::module(
                "governance_origin_required",
                format!("modules stub rejects {:?}", ctx.env().origin),
            ))
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Module for Modules {
        fn id(&self) -> ModuleId {
            self.id.clone()
        }

        fn root(&self) -> StateRoot {
            StateRoot::ZERO
        }

        fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
            Ok(StateSyncHandle::Stateless)
        }

        async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
            Self::authorize(ctx)?;
            match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
                ModulesMsg::RegisterModule {
                    module_id,
                    kind,
                    code_hash,
                    lanes: _,
                } => {
                    if self.entries.contains_key(&module_id) {
                        return Err(Error::module("module_exists", "module already registered"));
                    }
                    if code_hash.len() != CODE_HASH_LEN {
                        return Err(Error::module("bad_code_hash", "wrong code hash length"));
                    }
                    self.entries.insert(
                        module_id.clone(),
                        ModuleCode {
                            module_id,
                            kind,
                            active_code_hash: code_hash.clone(),
                            pending: None,
                            history: vec![Activation {
                                height: 0,
                                code_hash,
                            }],
                        },
                    );
                    Ok(())
                }
                ModulesMsg::ScheduleSwap {
                    name,
                    module_id,
                    activation_height,
                    code_hash,
                } => {
                    let Some(entry) = self.entries.get_mut(&module_id) else {
                        return Err(Error::module("unknown_module", "module is not registered"));
                    };
                    if entry.pending.is_some() {
                        return Err(Error::module(
                            "pending_swap",
                            "module already has a pending swap",
                        ));
                    }
                    if code_hash.len() != CODE_HASH_LEN {
                        return Err(Error::module("bad_code_hash", "wrong code hash length"));
                    }
                    entry.pending = Some(ScheduledSwap {
                        name,
                        activation_height,
                        code_hash,
                        readiness: Vec::new(),
                        ready_at: None,
                    });
                    Ok(())
                }
                ModulesMsg::ScheduleRegister {
                    name,
                    module_id,
                    kind,
                    activation_height,
                    code_hash,
                    lanes: _,
                } => {
                    if self.entries.contains_key(&module_id) {
                        return Err(Error::module("module_exists", "module already registered"));
                    }
                    if code_hash.len() != CODE_HASH_LEN {
                        return Err(Error::module("bad_code_hash", "wrong code hash length"));
                    }
                    self.entries.insert(
                        module_id.clone(),
                        ModuleCode {
                            module_id,
                            kind,
                            active_code_hash: Vec::new(),
                            pending: Some(ScheduledSwap {
                                name,
                                activation_height,
                                code_hash,
                                readiness: Vec::new(),
                                ready_at: None,
                            }),
                            history: Vec::new(),
                        },
                    );
                    Ok(())
                }
                ModulesMsg::CancelSwap { module_id, .. } => {
                    let Some(entry) = self.entries.get_mut(&module_id) else {
                        return Err(Error::module("unknown_module", "module is not registered"));
                    };
                    if entry.active_code_hash.is_empty() {
                        self.entries.remove(&module_id);
                    } else {
                        entry.pending = None;
                    }
                    Ok(())
                }
            }
        }

        async fn query(&self, request: &[u8]) -> Result<Vec<u8>, Error> {
            match decode_query(request).map_err(|e| Error::module("codec", e))? {
                ModulesQuery::ModuleStatus => Ok(encode_reply(&ModulesReply::ModuleStatus {
                    modules: self.entries.values().cloned().collect(),
                })),
                ModulesQuery::ArmedAt { .. } => {
                    Ok(encode_reply(&ModulesReply::ArmedAt { swaps: Vec::new() }))
                }
                ModulesQuery::Lanes => Ok(encode_reply(&ModulesReply::Lanes { lanes: Vec::new() })),
            }
        }
    }
}
