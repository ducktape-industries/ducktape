//! the identity module's public wire surface -- types only.
//!
//! an ACCOUNT is an abstract principal identified by a NUMBER (monotonic from
//! 1). how it is CONTROLLED ([`Control`]) decides which ops apply to it:
//!
//! - a KEY-HELD account ([`Control::Keys`]) owns an ASSOCIATION of keys of
//!   mixed [`KeyScheme`]s (an ed25519 device key, an Ethereum wallet, a
//!   WebAuthn passkey) and acts by signed frames: the frame ORIGIN is the
//!   acting key. [`IdentityMsg::Create`] founds an account for the origin,
//!   [`IdentityMsg::AddKey`] admits the origin into an existing member's
//!   account (that member proves consent over [`add_key_preimage`] at the
//!   origin's CURRENT generation, naming the ACCOUNT it admits into and the
//!   consensus time it dies at, so the proof is single-use, account-bound and
//!   short-lived), and `RemoveKey`/`SetName`/`SetProfile` need nothing but the
//!   origin's membership.
//! - a PROGRAM account ([`Control::Program`]) holds no key, ever. it is
//!   provisioned by a module ([`IdentityMsg::CreateProgram`], module-origin
//!   only: the emitting module becomes its EXECUTOR) on behalf of a CONTROLLER
//!   account, and acts only through calls its executor queued, which the host
//!   runs as `Origin::Program(account)` after checking the account's current
//!   control record. its GENERATION counts the mutations of that record; a
//!   call queued under an older generation is refused at execution, which is
//!   how an executor ([`IdentityMsg::SetProgramStanding`]) or a controller
//!   ([`IdentityMsg::TransferControl`]) invalidates everything queued before.
//!   [`IdentityMsg::RevokeProgram`] freezes it for good.
//! - a REVOKED account ([`Control::Revoked`]) is a former program: it never
//!   acts again and its record never changes again.
//!
//! no account is ever keyed by a key, no node is ever bound to an account, and
//! an account number is never spelled where a key goes: attribution comes from
//! a user-signed origin resolved through [`IdentityQuery::OfKey`], or from the
//! host-authenticated `Origin::Program(account)`.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

pub use keyscheme::KeyScheme;
pub use sdk::AccountNumber;
use sdk::ModuleId;

/// signing domain for add-key consents -- namespace-separated from every other
/// signed artifact (frames, invites, coord caps, endpoint records).
pub const IDENTITY_ADD_KEY_NS: &[u8] = b"ducktape-identity-add-key-v1";

/// max account name length, in bytes. a name is display text, not an id.
pub const MAX_NAME_LEN: usize = 64;
/// max key label length, in bytes (e.g. "Kim's phone").
pub const MAX_LABEL_LEN: usize = 64;
/// max account bio/status length, in bytes (a short status line).
pub const MAX_BIO_LEN: usize = 280;
/// max avatar reference length, in bytes -- a duckfs path
/// (`/shared/attachments/avatars/<sha16>.<ext>`); the bytes live in the files
/// module, identity stores only the reference.
pub const MAX_AVATAR_REF_LEN: usize = 512;
/// query pagination ceiling -- [`IdentityQuery::All`] and
/// [`IdentityQuery::Controlled`] clamp `limit` to this.
pub const MAX_QUERY_LIMIT: u64 = 256;
/// how far past the block executing it a consent's `expires_at` may reach, IN
/// WHATEVER UNIT THIS NETWORK'S `ConsensusTimePolicy` STAMPS `consensus_time`
/// IN -- this module compares `expires_at - now` against it directly, with no
/// unit conversion. On the validator/replica lanes `consensus_time` is the
/// block height, heartbeating at about one block a second, so this is roughly
/// seven days. On the sim lane's millisecond epoch clock the SAME NUMBER is
/// roughly ten minutes -- a client minting a consent must scale its own TTL
/// to that lane's unit (see `noded::ConsensusTimeUnit`), not assume this
/// ceiling means seven days everywhere. Long enough for a device pairing that
/// waits on a person, short enough that a mis-issued ticket is a mistake
/// rather than a permanent bearer credential to the account. a consent naming
/// a later expiry is refused at execution.
pub const MAX_CONSENT_TTL: u64 = 604_800;

/// one key of an association as queries expose it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyView {
    pub scheme: KeyScheme,
    pub pubkey: Vec<u8>,
    pub label: Option<String>,
    pub added_at: u64,
}

/// whether a program account's calls run. the executor sets it; the host
/// refuses a call of a suspended program at execution.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramStanding {
    Active,
    Suspended,
}

/// how an account is controlled -- the axis that decides which ops apply.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    /// a key-held account: its association ([`AccountView::keys`]) is never
    /// empty, and its members act by signed frames.
    Keys,
    /// a keyless account a module executes on a controller's behalf. the host
    /// runs one of its calls only when the call was queued by `executor`
    /// under this exact `generation` and `standing` is `Active`.
    Program {
        /// the account that may transfer or revoke control. never this
        /// account, and never one this account (transitively) controls.
        controller: AccountNumber,
        /// the module that provisioned the account: the only origin its calls
        /// are queued from, and the only one that sets its standing.
        executor: ModuleId,
        /// how many times the control record has changed since provisioning
        /// (a standing change or a control transfer); grows by exactly one
        /// per change, from 0.
        generation: u64,
        standing: ProgramStanding,
    },
    /// a former program account, frozen: it never acts and its record never
    /// changes again. `controller` is the account that held it at revocation.
    Revoked { controller: AccountNumber },
}

/// one account as queries expose it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountView {
    pub number: AccountNumber,
    /// display text; NOT unique -- the same person across workspaces is the
    /// same key in both associations, never the same name.
    pub name: String,
    pub control: Control,
    /// the association, ascending by public key. never empty for a key-held
    /// account; always empty for a program or revoked one.
    pub keys: Vec<KeyView>,
    pub avatar: Option<String>,
    pub bio: Option<String>,
    pub updated_at: u64,
}

/// an existing member's consent to admit the frame origin: which key, which
/// ACCOUNT it admits into, when the consent dies, and its scheme-owned proof
/// bytes over [`add_key_preimage`] at the origin's current generation.
///
/// `account` and `expires_at` are not spoofable: both are under the
/// authorizer's signature, and `account` is cross-checked against that key's
/// membership at execution. that pairing is the point -- the signature alone
/// would let a payload name an account nobody consented to, and the membership
/// lookup alone would follow the authorizer onto whatever account it sits on
/// when the consent is finally spent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Authorizer {
    pub key: Vec<u8>,
    /// the account this consent admits into -- the authorizer's own, at
    /// signing time and at execution time both.
    pub account: AccountNumber,
    /// consensus time after which the consent is refused; at most
    /// [`MAX_CONSENT_TTL`] past the block that executes it.
    pub expires_at: u64,
    pub proof: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityMsg {
    /// found a key-held account for the ORIGIN key (of `scheme`; the frame
    /// signature is the possession proof). the origin must belong to no
    /// account. stamps [`IdentityAssigned::Founded`].
    Create { name: String, scheme: KeyScheme },
    /// admit the ORIGIN key (of `scheme`) into `authorizer.account`, which the
    /// authorizer key must still belong to. `authorizer.proof` is that
    /// member's proof over [`add_key_preimage`] at the origin's CURRENT
    /// generation; on success the origin's generation advances, so the proof
    /// never verifies again. the origin must belong to no account.
    AddKey {
        scheme: KeyScheme,
        label: Option<String>,
        authorizer: Authorizer,
    },
    /// drop `key` from the origin's account. a member removes ITSELF, or a key
    /// admitted no earlier than it was -- never the last one, and never a key
    /// senior to the remover. writes nothing but the record: a removed key may
    /// be re-admitted later, at its next generation.
    RemoveKey { key: Vec<u8> },
    /// rename the acting account (a member key's, or the program account the
    /// host runs). empty trims reject; over [`MAX_NAME_LEN`] bytes rejects.
    SetName { name: String },
    /// set the avatar reference and/or bio of the acting account. each field
    /// empty-trims to cleared, over its byte cap rejects.
    SetProfile {
        avatar: Option<String>,
        bio: Option<String>,
    },
    /// MODULE-ORIGIN ONLY: found a program account executed by the emitting
    /// module on behalf of `controller`, which must be live (key-held, or an
    /// active program). the new account starts at generation 0, active, with
    /// no key. in the same unit, [`IdentityEvent::ProgramCreated`] carrying
    /// `request` is emitted back to the executor, so the executor's own
    /// binding commits with the account or not at all. stamps
    /// [`IdentityAssigned::Founded`].
    CreateProgram {
        name: String,
        controller: AccountNumber,
        request: u64,
    },
    /// EXECUTOR-ORIGIN ONLY: set a program's standing. every call advances the
    /// generation -- also when the standing is unchanged -- so an executor
    /// replacing or unbinding a program invalidates every call it queued
    /// before. rejects on a key-held or revoked account.
    SetProgramStanding {
        account: AccountNumber,
        standing: ProgramStanding,
    },
    /// CONTROLLER-ORIGIN ONLY: hand `account`'s control to `to`, which must be
    /// live and must not be `account` itself, its current controller, or an
    /// account whose control chain reaches `account`. advances the
    /// generation, so nothing queued under the previous controller stays
    /// executable. standing is untouched.
    TransferControl {
        account: AccountNumber,
        to: AccountNumber,
    },
    /// CONTROLLER-ORIGIN ONLY: freeze `account` as [`Control::Revoked`]. its
    /// generation stops here and no op ever touches the record again.
    RevokeProgram { account: AccountNumber },
}

/// An account number or an associated key whose current account is needed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountRef {
    Account(AccountNumber),
    Key(Vec<u8>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityQuery {
    /// accounts numbered `from..`, at most `limit` (clamped to
    /// [`MAX_QUERY_LIMIT`]). `from: 0` reads from 1. no account is ever
    /// deleted, so the numbering has no gaps.
    All {
        from: u64,
        limit: u64,
    },
    Get {
        number: AccountNumber,
    },
    /// the account `key` belongs to, if any -- the ONE resolver every consumer
    /// reads through. a program account holds no key, so it is never the
    /// answer here.
    OfKey {
        key: Vec<u8>,
    },
    /// Resolve at most [`MAX_QUERY_LIMIT`] references in their input order.
    /// Missing accounts/keys return `None`; an oversized batch rejects whole.
    Resolve {
        references: Vec<AccountRef>,
    },
    /// how many times `key` has been admitted anywhere (absent = 0) -- the
    /// generation an add-key consent must sign.
    KeyGen {
        key: Vec<u8>,
    },
    /// the accounts whose control record names `by` as controller (programs
    /// and revoked programs alike), numbered `from..`, at most `limit`
    /// (clamped to [`MAX_QUERY_LIMIT`]). `from: 0` reads from 1.
    Controlled {
        by: AccountNumber,
        from: u64,
        limit: u64,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityReply {
    Accounts(Vec<AccountView>),
    Account(Option<AccountView>),
    Resolved(Vec<Option<AccountNumber>>),
    Gen(u64),
}

/// a follow-up this module emits to another module, in the same unit as the
/// op that caused it. the receiver authenticates it by origin -- see
/// [`authenticate_event`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityEvent {
    /// [`IdentityMsg::CreateProgram`] founded `account` for `controller`;
    /// `request` is the executor's own correlation, echoed verbatim. sent to
    /// the executor only.
    ProgramCreated {
        request: u64,
        account: AccountNumber,
        controller: AccountNumber,
    },
}

/// the stamp an op declares through `set_assigned`: the value this module
/// assigned while applying it, which exists nowhere in the op payload.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityAssigned {
    /// the number [`IdentityMsg::Create`] or [`IdentityMsg::CreateProgram`]
    /// handed the new account.
    Founded { account: AccountNumber },
}

/// decode a follow-up as an [`IdentityEvent`] this module emitted. the ONE
/// authentication a receiver has is the dispatch origin: a genuine event
/// arrives as `Origin::Module(identity)`, the identity module's own id, and a
/// payload from any other origin is refused before it is decoded.
pub fn authenticate_event(
    origin: &sdk::Origin,
    identity: &str,
    payload: &[u8],
) -> Result<IdentityEvent, String> {
    let emitted_by_identity = matches!(origin, sdk::Origin::Module(module) if module == identity);
    if !emitted_by_identity {
        return Err(format!(
            "identity events are authenticated by origin: expected Module({identity:?}), got {origin:?}"
        ));
    }
    decode_event(payload)
}

/// the signed preimage of an add-key consent: chain id ‖ scheme tag ‖ new key
/// ‖ generation ‖ account ‖ expiry. the generation makes it single-use, the
/// account pins WHICH association it admits into (a consent minted on one
/// account never follows its author onto another), and the expiry bounds how
/// long an unspent one stays live -- there is no revoke op, so the clock is
/// the revocation.
pub fn add_key_preimage(
    chain_id: &str,
    scheme: KeyScheme,
    new_key: &[u8],
    generation: u64,
    account: AccountNumber,
    expires_at: u64,
) -> Vec<u8> {
    let mut out = Vec::new();
    sdk::codec::push_bytes(&mut out, chain_id.as_bytes());
    out.push(scheme.tag());
    sdk::codec::push_bytes(&mut out, new_key);
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&account.to_le_bytes());
    out.extend_from_slice(&expires_at.to_le_bytes());
    out
}

/// the byte principal other modules (governance ballots, forge owners) use
/// for an ACCOUNT: 8 bytes LE -- length-disjoint from every key scheme's
/// public key, so a principal is either an account or a key by length alone.
pub fn account_principal(number: AccountNumber) -> Vec<u8> {
    number.to_le_bytes().to_vec()
}

/// the inverse of [`account_principal`], for renderers: `Some(n)` iff `bytes`
/// is an 8-byte account principal.
pub fn principal_account(bytes: &[u8]) -> Option<AccountNumber> {
    <[u8; 8]>::try_from(bytes).ok().map(u64::from_le_bytes)
}

pub fn encode_msg(m: &IdentityMsg) -> Vec<u8> {
    sdk::wire::encode(m)
}
pub fn decode_msg(b: &[u8]) -> Result<IdentityMsg, String> {
    sdk::wire::decode(b)
}
pub fn encode_query(q: &IdentityQuery) -> Vec<u8> {
    sdk::wire::encode(q)
}
pub fn decode_query(b: &[u8]) -> Result<IdentityQuery, String> {
    sdk::wire::decode(b)
}
pub fn encode_reply(r: &IdentityReply) -> Vec<u8> {
    sdk::wire::encode(r)
}
pub fn decode_reply(b: &[u8]) -> Result<IdentityReply, String> {
    sdk::wire::decode(b)
}
pub fn encode_event(e: &IdentityEvent) -> Vec<u8> {
    sdk::wire::encode(e)
}
pub fn decode_event(b: &[u8]) -> Result<IdentityEvent, String> {
    sdk::wire::decode(b)
}
pub fn encode_assigned(a: &IdentityAssigned) -> Vec<u8> {
    sdk::wire::encode(a)
}
pub fn decode_assigned(b: &[u8]) -> Result<IdentityAssigned, String> {
    sdk::wire::decode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_key_preimage_is_deterministic_and_every_field_moves_it() {
        let base = add_key_preimage("net-a", KeyScheme::Ed25519, &[2u8; 32], 0, 1, 500);
        assert_eq!(
            base,
            add_key_preimage("net-a", KeyScheme::Ed25519, &[2u8; 32], 0, 1, 500)
        );
        assert_ne!(
            base,
            add_key_preimage("net-b", KeyScheme::Ed25519, &[2u8; 32], 0, 1, 500),
            "chain moves it"
        );
        assert_ne!(
            add_key_preimage("n", KeyScheme::Secp256k1, &[2u8; 33], 0, 1, 500),
            add_key_preimage("n", KeyScheme::Secp256r1, &[2u8; 33], 0, 1, 500),
            "scheme moves it"
        );
        assert_ne!(
            base,
            add_key_preimage("net-a", KeyScheme::Ed25519, &[3u8; 32], 0, 1, 500),
            "key moves it"
        );
        assert_ne!(
            base,
            add_key_preimage("net-a", KeyScheme::Ed25519, &[2u8; 32], 1, 1, 500),
            "generation moves it"
        );
        assert_ne!(
            base,
            add_key_preimage("net-a", KeyScheme::Ed25519, &[2u8; 32], 0, 2, 500),
            "account moves it"
        );
        assert_ne!(
            base,
            add_key_preimage("net-a", KeyScheme::Ed25519, &[2u8; 32], 0, 1, 501),
            "expiry moves it"
        );
    }

    #[test]
    fn account_principal_round_trips_and_never_looks_like_a_key() {
        assert_eq!(principal_account(&account_principal(7)), Some(7));
        assert_eq!(account_principal(1).len(), 8);
        assert_eq!(principal_account(&[0u8; 32]), None);
        assert_eq!(principal_account(&[0u8; 33]), None);
    }

    #[test]
    fn msg_codec_roundtrips() {
        let authorizer = Authorizer {
            key: vec![7; 32],
            account: 4,
            expires_at: 900,
            proof: vec![9; 64],
        };
        for m in [
            IdentityMsg::Create {
                name: "alice".into(),
                scheme: KeyScheme::Ed25519,
            },
            IdentityMsg::AddKey {
                scheme: KeyScheme::Secp256r1,
                label: Some("phone".into()),
                authorizer: authorizer.clone(),
            },
            IdentityMsg::RemoveKey { key: vec![4; 32] },
            IdentityMsg::SetName {
                name: "alice".into(),
            },
            IdentityMsg::SetProfile {
                avatar: Some("/shared/attachments/avatars/0123456789abcdef.png".into()),
                bio: None,
            },
            IdentityMsg::CreateProgram {
                name: "bot".into(),
                controller: 1,
                request: 9,
            },
            IdentityMsg::SetProgramStanding {
                account: 2,
                standing: ProgramStanding::Suspended,
            },
            IdentityMsg::TransferControl { account: 2, to: 3 },
            IdentityMsg::RevokeProgram { account: 2 },
        ] {
            assert_eq!(decode_msg(&encode_msg(&m)).unwrap(), m);
        }
        for q in [
            IdentityQuery::All { from: 0, limit: 16 },
            IdentityQuery::Get { number: 3 },
            IdentityQuery::OfKey { key: vec![3; 32] },
            IdentityQuery::KeyGen { key: vec![3; 32] },
            IdentityQuery::Controlled {
                by: 1,
                from: 0,
                limit: 16,
            },
        ] {
            assert_eq!(decode_query(&encode_query(&q)).unwrap(), q);
        }
        for r in [
            IdentityReply::Account(None),
            IdentityReply::Gen(4),
            IdentityReply::Accounts(vec![
                AccountView {
                    number: 1,
                    name: "a".into(),
                    control: Control::Keys,
                    keys: vec![KeyView {
                        scheme: KeyScheme::Ed25519,
                        pubkey: vec![1; 32],
                        label: None,
                        added_at: 0,
                    }],
                    avatar: None,
                    bio: None,
                    updated_at: 0,
                },
                AccountView {
                    number: 2,
                    name: "bot".into(),
                    control: Control::Program {
                        controller: 1,
                        executor: "agent".into(),
                        generation: 3,
                        standing: ProgramStanding::Active,
                    },
                    keys: Vec::new(),
                    avatar: None,
                    bio: None,
                    updated_at: 0,
                },
                AccountView {
                    number: 3,
                    name: "old-bot".into(),
                    control: Control::Revoked { controller: 1 },
                    keys: Vec::new(),
                    avatar: None,
                    bio: None,
                    updated_at: 0,
                },
            ]),
        ] {
            assert_eq!(decode_reply(&encode_reply(&r)).unwrap(), r);
        }
        let event = IdentityEvent::ProgramCreated {
            request: 9,
            account: 2,
            controller: 1,
        };
        assert_eq!(decode_event(&encode_event(&event)).unwrap(), event);
        let assigned = IdentityAssigned::Founded { account: 2 };
        assert_eq!(
            decode_assigned(&encode_assigned(&assigned)).unwrap(),
            assigned
        );
    }

    /// the receiver's check: only a follow-up whose origin IS the identity
    /// module decodes as an identity event.
    #[test]
    fn events_authenticate_by_origin_alone() {
        let event = IdentityEvent::ProgramCreated {
            request: 9,
            account: 2,
            controller: 1,
        };
        let payload = encode_event(&event);
        assert_eq!(
            authenticate_event(
                &sdk::Origin::Module("identity".into()),
                "identity",
                &payload
            )
            .unwrap(),
            event
        );
        for forged in [
            sdk::Origin::Module("agent".into()),
            sdk::Origin::External(vec![1; 32]),
            sdk::Origin::Program(2),
            sdk::Origin::System,
        ] {
            let refused = authenticate_event(&forged, "identity", &payload).unwrap_err();
            assert!(
                refused.contains("authenticated by origin"),
                "{forged:?} must be refused, got {refused}"
            );
        }
        assert!(
            authenticate_event(&sdk::Origin::Module("identity".into()), "identity", b"junk")
                .is_err(),
            "a genuine origin still needs a decodable payload"
        );
    }
}
