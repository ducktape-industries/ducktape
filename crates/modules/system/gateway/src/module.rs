//! The merged gateway module: the whole `.duck` **name → AccountId → route**
//! pipeline as ONE consensus tenant, qmdb-backed.
//!
//! Two planes share this module, both gated the same way: the frame origin is
//! a USER key of any scheme, and identity `OfKey` resolves it to the ACTING
//! ACCOUNT (a key of no account is refused; no node is bound to an account
//! anywhere — the ACL policy on the `gateway` target is the operator's
//! standing knob):
//! * the **handle plane** (absorbed from duckdns) — an optional human `.duck`
//!   name for one Identity account. Resolution stops at the account number;
//!   this module stores no node address.
//! * the **route plane** — an Identity account signs one monotonic route from
//!   its apex or a service label to a typed upstream plus an invocation policy,
//!   and owner-signed credential records ride beside the routes.
//!
//! ## State model
//!
//! pure logic over a host-injected [`sdk::MerkleStore`]: the HOST constructs
//! the concrete store (qmdb today — `statesync::qmdb::QmdbStore`) and hands it
//! to [`Gateway::new`], so this crate never names a storage crate. the record
//! families:
//!
//! * `handle\0{name}` → owning account number (borsh `u64`), with the 1:1
//!   inverse `owner\0{account LE8}` → handle (one handle per account BY
//!   CONSTRUCTION — the old full-map scan is now a point read), behind the
//!   sorted handle roster (`handles`, bounded by [`MAX_HANDLES`]) the
//!   paginated `Registrations` listing walks;
//! * `route\0{account LE8|flag|label}` → [`RouteRecord`] (borsh), behind a
//!   PER-ACCOUNT name roster (`routes\0{account LE8}`, bounded by
//!   [`MAX_ROUTES_PER_ACCOUNT`]) the `List` read walks — routes are never
//!   deleted (a `route: None` revision is the tombstone), so the roster only
//!   grows to its cap;
//! * `cred\0{name}` → [`CredentialRecord`], behind the sorted credential
//!   roster (`creds`, bounded by [`MAX_CREDENTIALS`]) the `Credentials`
//!   listing walks.
//!
//! writes are staged during a block and flushed to the store in one batch at
//! `commit_block`; the module root IS the store's merkle root. sync belongs to
//! the store, not this module: a joiner rebuilds the concrete store from a
//! peer (`QmdbStore::sync_from`) and wraps a fresh `Gateway` around it.
//!
//! oversized values never reach the store (the poison-value lesson): a route
//! record is byte-gated at [`MAX_RECORD_BYTES`] on top of
//! `validate_route_statement`'s JSON bound, a credential record is bounded by
//! construction ([`MAX_CREDENTIAL_GRANTS`] 8-byte grants), and every roster is
//! byte-gated on top of its count cap.
//!
//! ## Genesis config (the chain id)
//!
//! the per-network chain id reaches the NATIVE module through
//! [`Gateway::new`]. the wasm tenant is fixed bytes, so there the id rides
//! GENESIS CONFIG: the host seeds the reserved `__config` record
//! ([`sdk::genesis_config`]) into this module's store at genesis construction
//! — under [`sdk::store_key`] — and the guest decodes it per dispatch. the
//! config is consensus state in the store's merkle root from genesis and
//! rides state-sync like any other record. this module never writes that key.

use crate::identity_contract::{
    AccountView, IdentityQuery, IdentityReply, KeyView, decode_reply as identity_decode_reply,
    encode_query as identity_encode_query,
};
use borsh::{BorshDeserialize, BorshSerialize};
use duckdns::{HandleRegistration, ResolvedAccount, validate_handle};
use sdk::{
    Ctx, Error, MerkleStore, Module, ModuleId, Msg, Origin, ResolverSyncTarget, StagedStore,
    StateRoot, StateSyncHandle,
};

use crate::{
    CredentialGrantStatement, CredentialRecord, GATEWAY_CREDENTIAL_NS, GATEWAY_ROUTE_NS,
    GatewayMsg, GatewayQuery, GatewayReply, MAX_CREDENTIAL_GRANTS, MAX_QUERY_LIMIT,
    MAX_ROUTE_STATEMENT_JSON_BYTES, MAX_ROUTES_PER_ACCOUNT, MemberAuthorization,
    RemoveCredentialStatement, RouteName, RouteRecord, RouteStatement, RouteSummary,
    SetCredentialStatement, decode_msg, decode_query, encode_reply, grant_credential_preimage,
    remove_credential_preimage, revoke_credential_preimage, route_signing_preimage,
    set_credential_preimage, validate_account_number, validate_authorization,
    validate_credential_name, validate_route_statement,
};

/// route-record byte ceiling, enforced at every staged route write on top of
/// the statement's own JSON bound (the qmdb codec cap is decode-only).
pub const MAX_RECORD_BYTES: usize = MAX_ROUTE_STATEMENT_JSON_BYTES + 1024;
/// `.duck` handles retained at once (one per account, so this mirrors
/// identity's account cap). registering past it refuses loudly at execute.
pub const MAX_HANDLES: usize = 1024;
/// credential names retained at once (first registration wins a name).
pub const MAX_CREDENTIALS: usize = 1024;
/// roster byte backstop shared by the three roster records.
const MAX_ROSTER_RECORD_BYTES: usize = 512 * 1024;

/// per-handle record key: prefix + 0 + handle. safe because every key literal
/// below is fixed and none is another followed by a 0 byte.
fn handle_key(handle: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(6 + 1 + handle.len());
    key.extend_from_slice(b"handle");
    key.push(0);
    key.extend_from_slice(handle.as_bytes());
    key
}

/// the 1:1 inverse of [`handle_key`]: prefix + 0 + account LE8 → handle.
fn owner_key(account: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(5 + 1 + 8);
    key.extend_from_slice(b"owner");
    key.push(0);
    key.extend_from_slice(&account.to_le_bytes());
    key
}

/// per-route record key: prefix + 0 + account LE8 + the name (flag byte 0 =
/// apex, 1 + label bytes = named). the fixed-width account keeps the key
/// injective.
fn route_key(account: u64, name: &RouteName) -> Vec<u8> {
    let mut key = Vec::with_capacity(5 + 1 + 8 + 1 + 64);
    key.extend_from_slice(b"route");
    key.push(0);
    key.extend_from_slice(&account.to_le_bytes());
    match &name.label {
        None => key.push(0),
        Some(label) => {
            key.push(1);
            key.extend_from_slice(label.as_bytes());
        }
    }
    key
}

/// per-account route-name roster key: prefix + 0 + account LE8.
fn route_roster_key(account: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(6 + 1 + 8);
    key.extend_from_slice(b"routes");
    key.push(0);
    key.extend_from_slice(&account.to_le_bytes());
    key
}

/// per-credential record key: prefix + 0 + name.
fn cred_key(name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + 1 + name.len());
    key.extend_from_slice(b"cred");
    key.push(0);
    key.extend_from_slice(name.as_bytes());
    key
}

/// the handle roster's whole key (sorted handle names).
const HANDLE_ROSTER_KEY: &[u8] = b"handles";
/// the credential roster's whole key (sorted credential names).
const CRED_ROSTER_KEY: &[u8] = b"creds";

pub struct Gateway {
    id: ModuleId,
    identity_id: ModuleId,
    chain_id: String,
    /// the host-injected authenticated store plus this block's staging overlay
    /// (read-your-writes, folded into `root()` at `commit_block`). store key
    /// is `sha256(logical_key)`, owned by [`StagedStore`].
    staged: StagedStore,
}

impl Gateway {
    /// wrap the host-constructed store under module identity `id`.
    pub fn new(
        id: impl Into<ModuleId>,
        store: Box<dyn MerkleStore>,
        identity_id: impl Into<ModuleId>,
        chain_id: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            identity_id: identity_id.into(),
            chain_id: chain_id.into(),
            staged: StagedStore::new(store),
        }
    }

    // ---- staged-over-committed reads ----------------------------------------

    async fn load<T>(&self, key: &[u8]) -> Result<Option<T>, Error>
    where
        T: BorshDeserialize,
    {
        match self.staged.get(key).await? {
            Some(bytes) => Ok(Some(
                borsh::from_slice(&bytes).map_err(|e| Error::module("codec", e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// stage a value whose serialized size is bounded by construction (a
    /// handle/owner index entry, a credential record) — see the module doc's
    /// poison-value paragraph. route records and the rosters go through
    /// [`Self::store_bounded`].
    fn store<T>(&mut self, key: Vec<u8>, value: &T)
    where
        T: BorshSerialize,
    {
        self.staged.stage(
            key,
            borsh::to_vec(value).expect("gateway value is serializable"),
        );
    }

    /// stage a value only if its serialized size fits `cap` — the write-time
    /// guard against poison values (the qmdb codec cap is decode-only).
    fn store_bounded<T>(
        &mut self,
        key: Vec<u8>,
        value: &T,
        cap: usize,
        what: &str,
    ) -> Result<(), Error>
    where
        T: BorshSerialize,
    {
        let bytes = borsh::to_vec(value).expect("gateway value is serializable");
        if bytes.len() > cap {
            return Err(Error::module(
                "record_too_large",
                format!(
                    "gateway: {what} record too large: {} > {cap} bytes",
                    bytes.len()
                ),
            ));
        }
        self.staged.stage(key, bytes);
        Ok(())
    }

    /// the account owning `handle`, if registered.
    async fn handle_owner(&self, handle: &str) -> Result<Option<u64>, Error> {
        self.load(&handle_key(handle)).await
    }

    /// the handle `account` registered, if any — the 1:1 inverse index.
    async fn handle_of(&self, account: u64) -> Result<Option<String>, Error> {
        self.load(&owner_key(account)).await
    }

    /// the handle roster — every registered handle, sorted.
    async fn handle_roster(&self) -> Result<Vec<String>, Error> {
        Ok(self.load(HANDLE_ROSTER_KEY).await?.unwrap_or_default())
    }

    async fn route_record(
        &self,
        account: u64,
        name: &RouteName,
    ) -> Result<Option<RouteRecord>, Error> {
        self.load(&route_key(account, name)).await
    }

    /// the per-account route-name roster, sorted (apex before labels).
    async fn route_roster(&self, account: u64) -> Result<Vec<RouteName>, Error> {
        Ok(self
            .load(&route_roster_key(account))
            .await?
            .unwrap_or_default())
    }

    async fn credential_record(&self, name: &str) -> Result<Option<CredentialRecord>, Error> {
        self.load(&cred_key(name)).await
    }

    /// the credential roster — every registered credential name, sorted.
    async fn cred_roster(&self) -> Result<Vec<String>, Error> {
        Ok(self.load(CRED_ROSTER_KEY).await?.unwrap_or_default())
    }

    // ---- sibling reads --------------------------------------------------------

    /// the ACTING ACCOUNT: the one the origin key belongs to, through the one
    /// resolver every consumer reads (`OfKey`).
    async fn account_of_origin(&self, ctx: &dyn Ctx, origin: &[u8]) -> Result<AccountView, Error> {
        match identity_decode_reply(
            &ctx.query(
                &self.identity_id,
                &identity_encode_query(&IdentityQuery::OfKey {
                    key: origin.to_vec(),
                }),
            )
            .await?,
        )
        .map_err(|e| Error::module("identity_reply_decode", e))?
        {
            IdentityReply::Account(Some(account)) => Ok(account),
            IdentityReply::Account(None) => Err(Error::module(
                "no_identity_account",
                "gateway: origin key belongs to no Identity account",
            )),
        }
    }

    /// the authenticated submitter key: a non-empty external origin of any
    /// scheme (the frame signature already proved possession).
    fn origin_key(ctx: &dyn Ctx) -> Result<Vec<u8>, Error> {
        match &ctx.env().origin {
            Origin::External(key) if key.is_empty() => Err(Error::module(
                "empty_origin_key",
                "gateway: origin must be an external key",
            )),
            Origin::External(key) => Ok(key.clone()),
            other => Err(Error::module(
                "external_origin_required",
                format!("gateway: origin must be an external key, got {other:?}"),
            )),
        }
    }

    /// the shared authority gate of every signed statement: the statement's
    /// account must be the origin's, the signer a current member of it, and
    /// the proof must verify under the signer's STORED scheme.
    fn verify_member_signature(
        account: &AccountView,
        statement_account: u64,
        authorization: &MemberAuthorization,
        ns: &[u8],
        preimage: &[u8],
        what: &str,
    ) -> Result<(), Error> {
        if statement_account != account.number {
            return Err(Error::module(
                "not_the_origin_account",
                format!("gateway: {what} account is not the origin's account"),
            ));
        }
        let signer: &KeyView = account
            .keys
            .iter()
            .find(|key| key.pubkey == authorization.signer)
            .ok_or_else(|| {
                Error::module(
                    "not_a_member_key",
                    "gateway: signer is not a current account member",
                )
            })?;
        let verifies = signer.scheme.verify(
            &authorization.signer,
            ns,
            preimage,
            &authorization.signature,
        );
        if !verifies {
            return Err(Error::module(
                "signature_unverified",
                format!("gateway: {what} signature does not verify"),
            ));
        }
        Ok(())
    }

    // ---- the handle plane -------------------------------------------------------

    /// the handle plane: bind the origin's account to (or free) one optional
    /// `.duck` name. the account number is authority; the handle is a mutable
    /// presentation alias. renames are atomic; re-setting the current handle is
    /// an idempotent no-op that stages nothing.
    async fn set_handle(
        &mut self,
        ctx: &dyn Ctx,
        origin: &[u8],
        handle: Option<String>,
    ) -> Result<(), Error> {
        let account = self.account_of_origin(ctx, origin).await?.number;
        if let Some(handle) = &handle {
            validate_handle(handle).map_err(|e| Error::module("bad_handle", e))?;
            if self
                .handle_owner(handle)
                .await?
                .is_some_and(|owner| owner != account)
            {
                return Err(Error::module(
                    "handle_already_claimed",
                    format!("duckdns: handle {handle:?} is already claimed by another account"),
                ));
            }
        }

        let current = self.handle_of(account).await?;
        if current.as_deref() == handle.as_deref() {
            return Ok(());
        }

        let mut roster = self.handle_roster().await?;
        if let Some(current) = &current {
            self.staged.delete(handle_key(current));
            if let Ok(position) = roster.binary_search(current) {
                roster.remove(position);
            }
        }
        match handle {
            Some(handle) => {
                let position = match roster.binary_search(&handle) {
                    // the claimed-by-another check above admits only a re-claim
                    // by the same account, and that returned as a no-op — a
                    // rostered handle here is a store bug.
                    Ok(_) => {
                        return Err(Error::module(
                            "roster_corrupt",
                            "gateway: handle roster carries a name with no record",
                        ));
                    }
                    Err(position) => position,
                };
                if roster.len() >= MAX_HANDLES {
                    return Err(Error::module(
                        "handle_cap",
                        format!("gateway: handle cap reached ({MAX_HANDLES})"),
                    ));
                }
                roster.insert(position, handle.clone());
                self.store_bounded(
                    HANDLE_ROSTER_KEY.to_vec(),
                    &roster,
                    MAX_ROSTER_RECORD_BYTES,
                    "handle roster",
                )?;
                self.store(handle_key(&handle), &account);
                self.store(owner_key(account), &handle);
            }
            None => {
                if roster.is_empty() {
                    self.staged.delete(HANDLE_ROSTER_KEY.to_vec());
                } else {
                    self.store(HANDLE_ROSTER_KEY.to_vec(), &roster);
                }
                self.staged.delete(owner_key(account));
            }
        }
        Ok(())
    }

    // ---- the route plane ----------------------------------------------------------

    async fn set_route(
        &mut self,
        ctx: &dyn Ctx,
        origin: &[u8],
        statement: RouteStatement,
        authorization: MemberAuthorization,
    ) -> Result<(), Error> {
        if statement.chain_id != self.chain_id {
            return Err(Error::module(
                "foreign_chain",
                "gateway: route belongs to another chain",
            ));
        }
        // the account vouches for the node it names as publisher; the origin
        // is a user key, never compared to it.
        let account = self.account_of_origin(ctx, origin).await?;
        let preimage =
            route_signing_preimage(&statement).map_err(|e| Error::module("bad_route", e))?;
        Self::verify_member_signature(
            &account,
            statement.account_id,
            &authorization,
            GATEWAY_ROUTE_NS,
            &preimage,
            "route",
        )?;

        let record = RouteRecord {
            statement,
            authorization,
        };
        validate_route_statement(&record.statement).map_err(|e| Error::module("bad_route", e))?;
        validate_authorization(&record.authorization)
            .map_err(|e| Error::module("bad_authorization", e))?;
        let account_id = record.statement.account_id;
        let name = record.statement.name.clone();

        let existing = self.route_record(account_id, &name).await?;
        // the revision chain: 1 for a fresh name, current + 1 for a replace.
        let expected = match &existing {
            None => 1,
            Some(current) => current.statement.revision.checked_add(1).ok_or_else(|| {
                Error::module("revision_exhausted", "gateway: route revision is exhausted")
            })?,
        };
        if record.statement.revision != expected {
            return Err(Error::module(
                "stale_revision",
                format!(
                    "gateway: route revision must be {expected}, got {}",
                    record.statement.revision
                ),
            ));
        }
        if existing.is_none() {
            let mut roster = self.route_roster(account_id).await?;
            let Err(position) = roster.binary_search(&name) else {
                return Err(Error::module(
                    "roster_corrupt",
                    "gateway: route roster carries a name with no record",
                ));
            };
            if roster.len() >= MAX_ROUTES_PER_ACCOUNT {
                return Err(Error::module(
                    "route_cap",
                    format!("gateway: account route count exceeds {MAX_ROUTES_PER_ACCOUNT}"),
                ));
            }
            roster.insert(position, name.clone());
            self.store_bounded(
                route_roster_key(account_id),
                &roster,
                MAX_ROSTER_RECORD_BYTES,
                "route roster",
            )?;
        }
        self.store_bounded(
            route_key(account_id, &name),
            &record,
            MAX_RECORD_BYTES,
            "route",
        )
    }

    // ---- the credential plane -------------------------------------------------

    /// The shared credential authority check, the same gate as
    /// [`Self::set_route`]'s: `owner_account` must be the origin's account, the
    /// signer a current member key of it, and the proof must verify over
    /// `preimage` under [`GATEWAY_CREDENTIAL_NS`] with the signer's stored scheme.
    async fn verify_credential_owner(
        &self,
        ctx: &dyn Ctx,
        origin: &[u8],
        chain_id: &str,
        owner_account: u64,
        authorization: &MemberAuthorization,
        preimage: &[u8],
    ) -> Result<(), Error> {
        if chain_id != self.chain_id {
            return Err(Error::module(
                "foreign_chain",
                "gateway: credential belongs to another chain",
            ));
        }
        let account = self.account_of_origin(ctx, origin).await?;
        Self::verify_member_signature(
            &account,
            owner_account,
            authorization,
            GATEWAY_CREDENTIAL_NS,
            preimage,
            "credential",
        )
    }

    /// Load an owner's record for a grant/revoke/remove mutation, refusing when
    /// the name is unknown or owned by a different account.
    async fn owned_credential(
        &self,
        name: &str,
        owner_account: u64,
    ) -> Result<CredentialRecord, Error> {
        validate_credential_name(name).map_err(|e| Error::module("bad_credential_name", e))?;
        let record = self.credential_record(name).await?.ok_or_else(|| {
            Error::module(
                "unknown_credential",
                "gateway: credential is not registered",
            )
        })?;
        if record.owner_account != owner_account {
            return Err(Error::module(
                "not_the_credential_owner",
                "gateway: credential is owned by another account",
            ));
        }
        Ok(record)
    }

    async fn set_credential(
        &mut self,
        ctx: &dyn Ctx,
        origin: &[u8],
        statement: SetCredentialStatement,
        authorization: MemberAuthorization,
    ) -> Result<(), Error> {
        let preimage =
            set_credential_preimage(&statement).map_err(|e| Error::module("bad_credential", e))?;
        self.verify_credential_owner(
            ctx,
            origin,
            &statement.chain_id,
            statement.record.owner_account,
            &authorization,
            &preimage,
        )
        .await?;

        let mut record = statement.record;
        validate_credential_name(&record.name)
            .map_err(|e| Error::module("bad_credential_name", e))?;
        validate_account_number(record.owner_account)
            .map_err(|e| Error::module("bad_account", e))?;
        if !record.grants.is_empty() {
            return Err(Error::module(
                "no_grants",
                "gateway: credential registration carries no grants",
            ));
        }
        // first registration in consensus order wins the name: a record whose
        // owner differs from the committed one is refused.
        let existing = self.credential_record(&record.name).await?;
        if existing
            .as_ref()
            .is_some_and(|current| current.owner_account != record.owner_account)
        {
            return Err(Error::module(
                "credential_already_registered",
                "gateway: credential name already registered",
            ));
        }
        if existing.is_none() {
            let mut roster = self.cred_roster().await?;
            let Err(position) = roster.binary_search(&record.name) else {
                return Err(Error::module(
                    "roster_corrupt",
                    "gateway: credential roster carries a name with no record",
                ));
            };
            if roster.len() >= MAX_CREDENTIALS {
                return Err(Error::module(
                    "credential_cap",
                    format!("gateway: credential cap reached ({MAX_CREDENTIALS})"),
                ));
            }
            roster.insert(position, record.name.clone());
            self.store_bounded(
                CRED_ROSTER_KEY.to_vec(),
                &roster,
                MAX_ROSTER_RECORD_BYTES,
                "credential roster",
            )?;
        } else if let Some(current) = existing {
            // same-owner re-registration: the statement never carries grants
            // (refused above), so carry the committed grant set forward
            // instead of wiping it with the fresh, grant-free record.
            record.grants = current.grants;
        }
        // bounded by construction: scalar metadata + a capped grant set.
        self.store(cred_key(&record.name.clone()), &record);
        Ok(())
    }

    async fn remove_credential(
        &mut self,
        ctx: &dyn Ctx,
        origin: &[u8],
        statement: RemoveCredentialStatement,
        authorization: MemberAuthorization,
    ) -> Result<(), Error> {
        let preimage = remove_credential_preimage(&statement)
            .map_err(|e| Error::module("bad_credential", e))?;
        self.verify_credential_owner(
            ctx,
            origin,
            &statement.chain_id,
            statement.owner_account,
            &authorization,
            &preimage,
        )
        .await?;
        self.owned_credential(&statement.name, statement.owner_account)
            .await?;
        let mut roster = self.cred_roster().await?;
        if let Ok(position) = roster.binary_search(&statement.name) {
            roster.remove(position);
        }
        if roster.is_empty() {
            self.staged.delete(CRED_ROSTER_KEY.to_vec());
        } else {
            self.store(CRED_ROSTER_KEY.to_vec(), &roster);
        }
        self.staged.delete(cred_key(&statement.name));
        Ok(())
    }

    async fn grant_credential(
        &mut self,
        ctx: &dyn Ctx,
        origin: &[u8],
        statement: CredentialGrantStatement,
        authorization: MemberAuthorization,
    ) -> Result<(), Error> {
        let preimage = grant_credential_preimage(&statement)
            .map_err(|e| Error::module("bad_credential", e))?;
        self.verify_credential_owner(
            ctx,
            origin,
            &statement.chain_id,
            statement.owner_account,
            &authorization,
            &preimage,
        )
        .await?;
        validate_account_number(statement.account).map_err(|e| Error::module("bad_account", e))?;
        let mut record = self
            .owned_credential(&statement.name, statement.owner_account)
            .await?;
        let is_new_grant = record.grants.insert(statement.account);
        if is_new_grant && record.grants.len() > MAX_CREDENTIAL_GRANTS {
            return Err(Error::module(
                "grant_cap",
                format!("gateway: credential grant count exceeds {MAX_CREDENTIAL_GRANTS}"),
            ));
        }
        self.store(cred_key(&statement.name), &record);
        Ok(())
    }

    async fn revoke_credential(
        &mut self,
        ctx: &dyn Ctx,
        origin: &[u8],
        statement: CredentialGrantStatement,
        authorization: MemberAuthorization,
    ) -> Result<(), Error> {
        let preimage = revoke_credential_preimage(&statement)
            .map_err(|e| Error::module("bad_credential", e))?;
        self.verify_credential_owner(
            ctx,
            origin,
            &statement.chain_id,
            statement.owner_account,
            &authorization,
            &preimage,
        )
        .await?;
        let mut record = self
            .owned_credential(&statement.name, statement.owner_account)
            .await?;
        record.grants.remove(&statement.account);
        self.store(cred_key(&statement.name), &record);
        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl Module for Gateway {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    /// the store's merkle root over all committed records, verbatim — the
    /// staged overlay is invisible here until `commit_block`.
    fn root(&self) -> StateRoot {
        self.staged.root()
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        self.staged.state_sync_handle()
    }

    /// the network state-sync serve lane: answers the shared qmdb wire requests
    /// (historical proof-carrying op ranges) from committed state. read-only;
    /// the joiner's sync engine merkle-verifies every batch.
    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.staged.serve_sync(req).await
    }

    async fn resolver_sync_target(&self) -> Result<ResolverSyncTarget, Error> {
        self.staged.sync_target().await
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let origin = Self::origin_key(ctx)?;
        match decode_msg(&msg.payload).map_err(|e| Error::module("codec", e))? {
            GatewayMsg::SetHandle { handle } => self.set_handle(ctx, &origin, handle).await,
            GatewayMsg::SetRoute {
                statement,
                authorization,
            } => self.set_route(ctx, &origin, statement, authorization).await,
            GatewayMsg::SetCredential {
                statement,
                authorization,
            } => {
                self.set_credential(ctx, &origin, statement, authorization)
                    .await
            }
            GatewayMsg::RemoveCredential {
                statement,
                authorization,
            } => {
                self.remove_credential(ctx, &origin, statement, authorization)
                    .await
            }
            GatewayMsg::GrantCredential {
                statement,
                authorization,
            } => {
                self.grant_credential(ctx, &origin, statement, authorization)
                    .await
            }
            GatewayMsg::RevokeCredential {
                statement,
                authorization,
            } => {
                self.revoke_credential(ctx, &origin, statement, authorization)
                    .await
            }
        }
    }

    /// read projection — committed plus this block's staged changes (the
    /// staged-over-committed store view). the listings walk their rosters by
    /// derived key.
    async fn query(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        match decode_query(req).map_err(|e| Error::module("codec", e))? {
            GatewayQuery::Resolve { name } => {
                name.validate().map_err(|e| Error::module("bad_name", e))?;
                let resolved = self
                    .handle_owner(&name.handle)
                    .await?
                    .map(|account_id| ResolvedAccount { account_id });
                Ok(encode_reply(&GatewayReply::Resolved(resolved)))
            }
            GatewayQuery::Registrations { from, limit } => {
                if limit > MAX_QUERY_LIMIT {
                    return Err(Error::module(
                        "query_limit",
                        format!(
                            "duckdns: registration query limit {limit} exceeds {MAX_QUERY_LIMIT}"
                        ),
                    ));
                }
                let from = usize::try_from(from).unwrap_or(usize::MAX);
                let mut registrations = Vec::new();
                for handle in self
                    .handle_roster()
                    .await?
                    .iter()
                    .skip(from)
                    .take(limit as usize)
                {
                    let account_id = self.handle_owner(handle).await?.ok_or_else(|| {
                        Error::module(
                            "roster_corrupt",
                            "gateway: handle roster carries a name with no record",
                        )
                    })?;
                    registrations.push(HandleRegistration {
                        handle: handle.clone(),
                        account_id,
                    });
                }
                Ok(encode_reply(&GatewayReply::Registrations(registrations)))
            }
            GatewayQuery::Get { account_id, name } => {
                validate_account_number(account_id).map_err(|e| Error::module("bad_account", e))?;
                name.validate().map_err(|e| Error::module("bad_name", e))?;
                Ok(encode_reply(&GatewayReply::Route(Box::new(
                    self.route_record(account_id, &name).await?,
                ))))
            }
            GatewayQuery::List { account_id } => {
                validate_account_number(account_id).map_err(|e| Error::module("bad_account", e))?;
                let mut routes = Vec::new();
                for name in self.route_roster(account_id).await? {
                    let record = self.route_record(account_id, &name).await?.ok_or_else(|| {
                        Error::module(
                            "roster_corrupt",
                            "gateway: route roster carries a name with no record",
                        )
                    })?;
                    let Some(route) = record.statement.route.as_ref() else {
                        continue;
                    };
                    routes.push(RouteSummary {
                        name: record.statement.name.clone(),
                        publisher_node: record.statement.publisher_node.clone(),
                        revision: record.statement.revision,
                        target: route.target.kind_name().to_string(),
                    });
                }
                Ok(encode_reply(&GatewayReply::Routes(routes)))
            }
            GatewayQuery::Credential { name } => {
                validate_credential_name(&name)
                    .map_err(|e| Error::module("bad_credential_name", e))?;
                Ok(encode_reply(&GatewayReply::Credential(
                    self.credential_record(&name).await?,
                )))
            }
            GatewayQuery::Credentials {} => {
                let mut credentials = Vec::new();
                for name in self.cred_roster().await? {
                    let record = self.credential_record(&name).await?.ok_or_else(|| {
                        Error::module(
                            "roster_corrupt",
                            "gateway: credential roster carries a name with no record",
                        )
                    })?;
                    credentials.push(record);
                }
                Ok(encode_reply(&GatewayReply::Credentials(credentials)))
            }
        }
    }

    /// publish the block's staged writes in ONE store batch. no-op (and no
    /// root movement) if nothing was staged.
    async fn commit_block(&mut self) -> Result<(), Error> {
        self.staged.commit().await
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged.abort();
        Ok(())
    }
}
