//! Experimental stateful EVM backed by QMDB rather than Ethereum's MPT.
//!
//! The module executes ordinary Prague create/call transactions through REVM.
//! Its canonical account, code, and storage snapshot lives under one key in the
//! existing QMDB [`kv::Kv`], whose merkle root is this module's [`StateRoot`].

mod interface;
pub use interface::*;

pub mod index;

use kv::Kv;
use revm::{
    Context as RevmContext, ExecuteCommitEvm, MainBuilder, MainContext,
    bytecode::Bytecode,
    context::{TxEnv, result::ExecutionResult},
    database::{AccountState, InMemoryDB},
    primitives::{Address, B256, KECCAK_EMPTY, Log, TxKind, U256, hardfork::SpecId, keccak256},
    state::AccountInfo,
};
use sdk::{
    Ctx, Error, Event, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StateRoot,
    StateSyncHandle,
};
use serde::{Deserialize, Serialize};

const STATE_KEY: &[u8] = b"evm/state/v1";
pub const MAX_INIT_CODE_BYTES: usize = 49_152;
pub const MAX_INPUT_BYTES: usize = 128 * 1024;
pub const MAX_GAS_LIMIT: u64 = 10_000_000;

#[derive(Serialize, Deserialize)]
struct SnapshotAccount {
    address: [u8; 20],
    balance: [u8; 32],
    nonce: u64,
    code: Vec<u8>,
    storage: Vec<SnapshotSlot>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotSlot {
    key: [u8; 32],
    value: [u8; 32],
}

pub struct EvmModule {
    id: ModuleId,
    store: Kv,
    committed: InMemoryDB,
    pending: Option<InMemoryDB>,
}

impl EvmModule {
    /// Wrap the host-injected merkle store (the host constructs the concrete
    /// qmdb store and hands it in as `Box<dyn MerkleStore>`).
    pub async fn init(id: impl Into<ModuleId>, store: Box<dyn MerkleStore>) -> Result<Self, Error> {
        let id = id.into();
        let store = Kv::new(id.clone(), store);
        Self::from_store(id, store).await
    }

    /// Rehydrate the EVM adapter around an already verified QMDB store. State
    /// sync uses this after `QmdbStore::sync_from` reconstructs the module root.
    pub async fn from_store(id: impl Into<ModuleId>, store: Kv) -> Result<Self, Error> {
        let id = id.into();
        let committed = match store.get(STATE_KEY).await {
            Some(bytes) => decode_snapshot(&bytes)?,
            None => InMemoryDB::default(),
        };
        Ok(Self {
            id,
            store,
            committed,
            pending: None,
        })
    }

    fn active(&self) -> &InMemoryDB {
        self.pending.as_ref().unwrap_or(&self.committed)
    }

    fn run(
        mut db: InMemoryDB,
        tx: EvmTx,
        origin: &Origin,
        height: u64,
        consensus_time: u64,
    ) -> Result<(EvmResult, InMemoryDB), Error> {
        let (kind, data, gas_limit) = match tx {
            EvmTx::Create {
                init_code,
                gas_limit,
            } => {
                if init_code.len() > MAX_INIT_CODE_BYTES {
                    return Err(Error::module(
                        "init_code_too_large",
                        format!(
                            "EVM init code too large: {} bytes exceeds {MAX_INIT_CODE_BYTES}",
                            init_code.len()
                        ),
                    ));
                }
                (TxKind::Create, init_code, gas_limit)
            }
            EvmTx::Call {
                to,
                input,
                gas_limit,
            } => {
                if input.len() > MAX_INPUT_BYTES {
                    return Err(Error::module(
                        "input_too_large",
                        format!(
                            "EVM input too large: {} bytes exceeds {MAX_INPUT_BYTES}",
                            input.len()
                        ),
                    ));
                }
                (TxKind::Call(Address::from(to)), input, gas_limit)
            }
        };
        if !(21_000..=MAX_GAS_LIMIT).contains(&gas_limit) {
            return Err(Error::module(
                "bad_gas_limit",
                format!("EVM gas limit must be between 21000 and {MAX_GAS_LIMIT}"),
            ));
        }

        let caller = origin_address(origin);
        let nonce = db
            .cache
            .accounts
            .get(&caller)
            .filter(|account| account.account_state != AccountState::NotExisting)
            .map(|account| account.info.nonce);
        let nonce = nonce.unwrap_or_else(|| {
            db.insert_account_info(
                caller,
                AccountInfo {
                    balance: U256::MAX,
                    ..Default::default()
                },
            );
            0
        });

        let context = RevmContext::mainnet()
            .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(SpecId::PRAGUE))
            .modify_block_chained(|block| {
                block.number = U256::from(height);
                block.timestamp = U256::from(consensus_time);
            })
            .with_db(db);
        let tx = TxEnv::builder()
            .caller(caller)
            .kind(kind)
            .gas_limit(gas_limit)
            .data(data.into())
            .nonce(nonce)
            .chain_id(None)
            .build()
            .map_err(|e| {
                Error::module("bad_transaction", format!("invalid EVM transaction: {e}"))
            })?;
        let mut evm = context.build_mainnet();
        let result = evm.transact_commit(tx).map_err(|e| {
            Error::module("transact_commit", format!("EVM transaction rejected: {e}"))
        })?;
        let db = evm.ctx.journaled_state.database;
        Ok((execution_result(result), db))
    }
}

#[async_trait::async_trait(?Send)]
impl Module for EvmModule {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn root(&self) -> StateRoot {
        self.store.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.store.state_sync_handle()
    }

    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.store.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.store.resolver_sync_target().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
            EvmMsg::Execute(transaction) => {
                let env = ctx.env();
                let caller = address_bytes(&origin_address(&env.origin));
                let (result, db) = Self::run(
                    self.active().clone(),
                    transaction.clone(),
                    &env.origin,
                    env.height,
                    env.consensus_time,
                )?;
                // ponytail: one whole-state QMDB value keeps the experiment tiny; split
                // accounts/storage into individual keys when the 1 MiB Kv value cap matters.
                self.store
                    .stage(STATE_KEY.to_vec(), encode_snapshot(&db)?)?;
                self.pending = Some(db);
                ctx.emit_msg(Msg {
                    target: self.id.clone(),
                    payload: encode_msg(&EvmMsg::Receipt {
                        transaction,
                        caller,
                        result,
                    }),
                });
                Ok(())
            }
            EvmMsg::Receipt { result, .. } => {
                let env = ctx.env();
                if env.origin != Origin::Module(self.id.clone()) {
                    return Err(Error::module(
                        "module_origin_required",
                        "EVM receipts may only be emitted by the EVM module",
                    ));
                }
                ctx.emit_event(Event {
                    source: self.id.clone(),
                    payload: encode_result(&result),
                });
                Ok(())
            }
        }
    }

    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        let EvmQuery::Simulate(tx) = decode_query(req).map_err(|e| Error::module("codec", e))?;
        Self::run(self.active().clone(), tx, &Origin::System, 0, 0)
            .map(|(result, _)| encode_result(&result))
    }

    async fn query_with(&self, ctx: &dyn Ctx, req: &[u8]) -> Result<Vec<u8>, Error> {
        let EvmQuery::Simulate(tx) = decode_query(req).map_err(|e| Error::module("codec", e))?;
        let env = ctx.env();
        Self::run(
            self.active().clone(),
            tx,
            &env.origin,
            env.height,
            env.consensus_time,
        )
        .map(|(result, _)| encode_result(&result))
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        self.store.commit_block().await?;
        if let Some(db) = self.pending.take() {
            self.committed = db;
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.store.abort_block().await?;
        self.pending = None;
        Ok(())
    }
}

fn origin_address(origin: &Origin) -> Address {
    let mut preimage = Vec::new();
    match origin {
        Origin::External(id) => {
            preimage.extend_from_slice(b"external\0");
            preimage.extend_from_slice(id);
        }
        Origin::Module(id) => {
            preimage.extend_from_slice(b"module\0");
            preimage.extend_from_slice(id.as_bytes());
        }
        Origin::Program(account) => {
            preimage.extend_from_slice(b"acct\0");
            preimage.extend_from_slice(&account.to_le_bytes());
        }
        Origin::System => preimage.extend_from_slice(b"system"),
    }
    let hash = keccak256(preimage);
    Address::from_slice(&hash[12..])
}

fn execution_result(result: ExecutionResult) -> EvmResult {
    match result {
        ExecutionResult::Success {
            gas, output, logs, ..
        } => EvmResult {
            status: EvmStatus::Success,
            gas_used: gas.tx_gas_used(),
            created_address: output.address().map(address_bytes),
            output: output.into_data().to_vec(),
            logs: logs.into_iter().map(evm_log).collect(),
        },
        ExecutionResult::Revert {
            gas, output, logs, ..
        } => EvmResult {
            status: EvmStatus::Revert,
            gas_used: gas.tx_gas_used(),
            output: output.to_vec(),
            created_address: None,
            logs: logs.into_iter().map(evm_log).collect(),
        },
        ExecutionResult::Halt {
            reason, gas, logs, ..
        } => EvmResult {
            status: EvmStatus::Halt {
                reason: format!("{reason:?}"),
            },
            gas_used: gas.tx_gas_used(),
            output: Vec::new(),
            created_address: None,
            logs: logs.into_iter().map(evm_log).collect(),
        },
    }
}

fn evm_log(log: Log) -> EvmLog {
    let (topics, data) = log.data.split();
    EvmLog {
        address: address_bytes(&log.address),
        topics: topics.iter().map(word_bytes).collect(),
        data: data.to_vec(),
    }
}

fn encode_snapshot(db: &InMemoryDB) -> Result<Vec<u8>, Error> {
    let mut accounts: Vec<_> = db
        .cache
        .accounts
        .iter()
        .filter(|(_, account)| account.account_state != AccountState::NotExisting)
        .filter(|(_, account)| {
            !account.info.is_empty() || account.storage.values().any(|value| !value.is_zero())
        })
        .collect();
    accounts.sort_by_key(|(address, _)| **address);

    let accounts: Result<Vec<_>, Error> = accounts
        .into_iter()
        .map(|(address, account)| {
            let code = account
                .info
                .code
                .as_ref()
                .or_else(|| db.cache.contracts.get(&account.info.code_hash))
                .map(|code| code.original_byte_slice().to_vec())
                .or_else(|| (account.info.code_hash == KECCAK_EMPTY).then(Vec::new))
                .ok_or_else(|| {
                    Error::module("code_absent", "EVM account code missing from cache")
                })?;
            let mut storage: Vec<_> = account
                .storage
                .iter()
                .filter(|(_, value)| !value.is_zero())
                .map(|(key, value)| SnapshotSlot {
                    key: key.to_be_bytes(),
                    value: value.to_be_bytes(),
                })
                .collect();
            storage.sort_by_key(|slot| slot.key);
            Ok(SnapshotAccount {
                address: address_bytes(address),
                balance: account.info.balance.to_be_bytes(),
                nonce: account.info.nonce,
                code,
                storage,
            })
        })
        .collect();
    serde_json::to_vec(&accounts?).map_err(|e| Error::module("snapshot_encode", e.to_string()))
}

fn decode_snapshot(bytes: &[u8]) -> Result<InMemoryDB, Error> {
    let accounts: Vec<SnapshotAccount> = serde_json::from_slice(bytes)
        .map_err(|e| Error::module("snapshot_decode", e.to_string()))?;
    if accounts
        .windows(2)
        .any(|pair| pair[0].address >= pair[1].address)
    {
        return Err(Error::module(
            "snapshot_decode",
            "EVM snapshot accounts are not strictly sorted",
        ));
    }

    let mut db = InMemoryDB::default();
    for account in accounts {
        if account
            .storage
            .windows(2)
            .any(|pair| pair[0].key >= pair[1].key)
        {
            return Err(Error::module(
                "snapshot_decode",
                "EVM snapshot storage is not strictly sorted",
            ));
        }
        let address = Address::from(account.address);
        let code = Bytecode::new_raw_checked(account.code.into()).map_err(|e| {
            Error::module(
                "snapshot_decode",
                format!("invalid EVM snapshot bytecode: {e}"),
            )
        })?;
        db.insert_account_info(
            address,
            AccountInfo::default()
                .with_balance(U256::from_be_bytes(account.balance))
                .with_nonce(account.nonce)
                .with_code(code),
        );
        for slot in account.storage {
            db.insert_account_storage(
                address,
                U256::from_be_bytes(slot.key),
                U256::from_be_bytes(slot.value),
            )
            .expect("the in-memory EVM database is infallible");
        }
    }
    Ok(db)
}

fn address_bytes(address: &Address) -> [u8; 20] {
    let mut bytes = [0; 20];
    bytes.copy_from_slice(address.as_slice());
    bytes
}

fn word_bytes(word: &B256) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes.copy_from_slice(word.as_slice());
    bytes
}

#[cfg(test)]
mod tests {
    use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
    use host::{BlockContext, Host};
    use statesync::qmdb::QmdbStore;

    use super::*;

    fn storage_contract_init_code() -> Vec<u8> {
        let runtime = [
            0x36, 0x15, 0x60, 0x0c, 0x57, 0x60, 0x00, 0x35, 0x60, 0x00, 0x55, 0x00, 0x5b, 0x60,
            0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
        ];
        [
            &[
                0x60,
                runtime.len() as u8,
                0x60,
                0x0c,
                0x60,
                0x00,
                0x39,
                0x60,
            ][..],
            &[runtime.len() as u8, 0x60, 0x00, 0xf3],
            &runtime,
        ]
        .concat()
    }

    #[test]
    fn deploys_and_persists_contract_storage_in_qmdb() {
        deterministic::Runner::default().start(|context| async move {
            let store = QmdbStore::init(context.child("evm"), "evm").await;
            let evm = EvmModule::init("evm", Box::new(store)).await.unwrap();
            let mut host = Host::genesis(vec![Box::new(evm)]).unwrap();
            let root_before = host.module_root("evm").unwrap();

            let deploy = host
                .submit(Msg {
                    target: "evm".into(),
                    payload: encode_msg(&EvmMsg::Execute(EvmTx::Create {
                        init_code: storage_contract_init_code(),
                        gas_limit: 500_000,
                    })),
                })
                .await
                .unwrap();
            let deployed = decode_result(&deploy.events[0].payload).unwrap();
            assert_eq!(deployed.status, EvmStatus::Success);
            let contract = deployed.created_address.unwrap();
            assert_eq!(deploy.dispatches.len(), 2);
            assert_eq!(deploy.dispatches[1].origin, Origin::Module("evm".into()));
            let receipt = decode_msg(&deploy.dispatches[1].payload).unwrap();
            let EvmMsg::Receipt { result, .. } = &receipt else {
                panic!("the EVM follow-up carries its receipt")
            };
            assert_eq!(result, &deployed);

            let root_before_forged_receipt = host.module_root("evm").unwrap();
            host.submit_at(
                BlockContext {
                    origin: Origin::External(b"forger".to_vec()),
                    ..Default::default()
                },
                Msg {
                    target: "evm".into(),
                    payload: encode_msg(&receipt),
                },
            )
            .await
            .expect_err("an external caller cannot forge an indexed receipt");
            assert_eq!(host.module_root("evm").unwrap(), root_before_forged_receipt);
            let root_after_deploy = host.module_root("evm").unwrap();
            assert_ne!(root_before, root_after_deploy);

            let mut value = vec![0; 32];
            value[31] = 42;
            host.submit(Msg {
                target: "evm".into(),
                payload: encode_msg(&EvmMsg::Execute(EvmTx::Call {
                    to: contract,
                    input: value.clone(),
                    gas_limit: 100_000,
                })),
            })
            .await
            .unwrap();
            assert_ne!(host.module_root("evm").unwrap(), root_after_deploy);

            let reply = host
                .query(
                    "evm",
                    &encode_query(&EvmQuery::Simulate(EvmTx::Call {
                        to: contract,
                        input: Vec::new(),
                        gas_limit: 100_000,
                    })),
                )
                .await
                .unwrap();
            let reply = decode_result(&reply).unwrap();
            assert_eq!(reply.status, EvmStatus::Success);
            assert_eq!(reply.output, value);

            let committed_root = host.module_root("evm").unwrap();
            drop(host);
            let store = QmdbStore::init(context.child("evm"), "evm").await;
            let reopened = EvmModule::init("evm", Box::new(store)).await.unwrap();
            assert_eq!(reopened.root(), committed_root);
            let reopened = Host::genesis(vec![Box::new(reopened)]).unwrap();
            let reply = reopened
                .query(
                    "evm",
                    &encode_query(&EvmQuery::Simulate(EvmTx::Call {
                        to: contract,
                        input: Vec::new(),
                        gas_limit: 100_000,
                    })),
                )
                .await
                .unwrap();
            assert_eq!(decode_result(&reply).unwrap().output, value);
        });
    }
}
