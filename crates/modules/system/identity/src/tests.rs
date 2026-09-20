//! account-model tests: founding by number, multi-scheme associations, the
//! single-use add-key consent (per-key generation), re-linking a removed key,
//! the last-key guard, member gating of rename/profile, the cap, and abort;
//! then program accounts: module-only provisioning with its same-unit
//! follow-up, keyless lookup, executor- and controller-gated ops, generation
//! invalidation and exhaustion, transfer and cycle refusal, revocation, and
//! the `Controlled` page — all over the store-backed module (a [`MemStore`]
//! test double; the qmdb continuity proof lives in `tests/sync_round_trip.rs`).

use super::*;
use commonware_cryptography::Signer as _;
use futures::executor::block_on;
use sdk::{Cause, Env};
use sdk_testkit::{MemStore, TestCtx};

const CHAIN: &str = "test-chain";
/// the consensus time every op runs at unless a test names another.
const NOW: u64 = 100;
/// the expiry every consent carries unless a test names another -- inside
/// [`MAX_CONSENT_TTL`] of [`NOW`].
const EXPIRES: u64 = 1_000;

// ---- a minimal Ctx (the shared sdk-testkit double) ----------------------

fn ctx_with(origin: Origin, consensus_time: u64) -> TestCtx {
    TestCtx::with_env(Env {
        height: 0,
        consensus_time,
        origin,
        me: "identity".into(),
        cause: Cause::Direct,
    })
}

fn from_key(key: &[u8]) -> Origin {
    Origin::External(key.to_vec())
}

fn from_module(module: &str) -> Origin {
    Origin::Module(module.into())
}

fn as_program(account: u64) -> Origin {
    Origin::Program(account)
}

fn ctx_at(key: &[u8], consensus_time: u64) -> TestCtx {
    ctx_with(from_key(key), consensus_time)
}

fn ctx_external(key: &[u8]) -> TestCtx {
    ctx_at(key, NOW)
}

// ---- key builders (one per scheme) -----------------------------------------

type Ed = commonware_cryptography::ed25519::PrivateKey;

fn ed(seed: u64) -> Ed {
    Ed::from_seed(seed)
}
fn ed_pub(k: &Ed) -> Vec<u8> {
    k.public_key().as_ref().to_vec()
}
/// an ed25519 member's consent to admit `new_key` into `account` at `gen`.
fn ed_consent(
    member: &Ed,
    scheme: KeyScheme,
    new_key: &[u8],
    generation: u64,
    account: u64,
) -> Authorizer {
    ed_consent_until(member, scheme, new_key, generation, account, EXPIRES)
}

fn ed_consent_until(
    member: &Ed,
    scheme: KeyScheme,
    new_key: &[u8],
    generation: u64,
    account: u64,
    expires_at: u64,
) -> Authorizer {
    let preimage = add_key_preimage(CHAIN, scheme, new_key, generation, account, expires_at);
    Authorizer {
        key: ed_pub(member),
        account,
        expires_at,
        proof: keyscheme::testkit::ed25519_proof(member, IDENTITY_ADD_KEY_NS, &preimage),
    }
}

fn wallet(seed: u8) -> k256::ecdsa::SigningKey {
    keyscheme::testkit::eth_key(seed)
}
fn wallet_pub(k: &k256::ecdsa::SigningKey) -> Vec<u8> {
    keyscheme::testkit::eth_pubkey(k)
}
fn wallet_consent(
    member: &k256::ecdsa::SigningKey,
    scheme: KeyScheme,
    new_key: &[u8],
    generation: u64,
    account: u64,
) -> Authorizer {
    let preimage = add_key_preimage(CHAIN, scheme, new_key, generation, account, EXPIRES);
    Authorizer {
        key: wallet_pub(member),
        account,
        expires_at: EXPIRES,
        proof: keyscheme::testkit::eth_proof(member, IDENTITY_ADD_KEY_NS, &preimage),
    }
}

fn passkey(seed: u8) -> p256::ecdsa::SigningKey {
    keyscheme::testkit::passkey(seed)
}
fn passkey_pub(k: &p256::ecdsa::SigningKey) -> Vec<u8> {
    keyscheme::testkit::passkey_pubkey(k)
}
fn passkey_consent(
    member: &p256::ecdsa::SigningKey,
    scheme: KeyScheme,
    new_key: &[u8],
    generation: u64,
    account: u64,
) -> Authorizer {
    let preimage = add_key_preimage(CHAIN, scheme, new_key, generation, account, EXPIRES);
    Authorizer {
        key: passkey_pub(member),
        account,
        expires_at: EXPIRES,
        proof: keyscheme::testkit::passkey_proof(
            member,
            "ducktape",
            IDENTITY_ADD_KEY_NS,
            &preimage,
            true,
        ),
    }
}

// ---- harness ------------------------------------------------------------

fn new_identity() -> Identity {
    Identity::new("identity", Box::new(MemStore::new()), CHAIN.to_string())
}

fn identity_msg(msg: &IdentityMsg) -> Msg {
    Msg {
        target: "identity".into(),
        payload: encode_msg(msg),
    }
}

/// execute a message from `origin` at `at`, then commit (or abort) the block,
/// handing back the ctx so a test can read what the op emitted and stamped.
fn apply_from_at(
    id: &mut Identity,
    origin: Origin,
    msg: IdentityMsg,
    at: u64,
) -> Result<TestCtx, Error> {
    let mut ctx = ctx_with(origin, at);
    match block_on(id.execute(&mut ctx, &identity_msg(&msg))) {
        Ok(()) => {
            block_on(id.commit_block()).unwrap();
            Ok(ctx)
        }
        Err(e) => {
            block_on(id.abort_block()).unwrap();
            Err(e)
        }
    }
}

fn apply_from(id: &mut Identity, origin: Origin, msg: IdentityMsg) -> Result<TestCtx, Error> {
    apply_from_at(id, origin, msg, NOW)
}

/// execute a message from the key `origin`, then commit (or abort) the block.
fn apply(id: &mut Identity, origin: &[u8], msg: IdentityMsg) -> Result<(), Error> {
    apply_at(id, origin, msg, NOW)
}

/// [`apply`] at a chosen consensus time — how a test orders admissions.
fn apply_at(id: &mut Identity, origin: &[u8], msg: IdentityMsg, at: u64) -> Result<(), Error> {
    apply_from_at(id, from_key(origin), msg, at).map(|_| ())
}

fn create(id: &mut Identity, origin: &[u8], name: &str, scheme: KeyScheme) -> Result<(), Error> {
    apply(
        id,
        origin,
        IdentityMsg::Create {
            name: name.into(),
            scheme,
        },
    )
}

fn add_key(
    id: &mut Identity,
    origin: &[u8],
    scheme: KeyScheme,
    authorizer: Authorizer,
) -> Result<(), Error> {
    add_key_at(id, origin, scheme, authorizer, NOW)
}

fn add_key_at(
    id: &mut Identity,
    origin: &[u8],
    scheme: KeyScheme,
    authorizer: Authorizer,
    at: u64,
) -> Result<(), Error> {
    apply_at(
        id,
        origin,
        IdentityMsg::AddKey {
            scheme,
            label: None,
            authorizer,
        },
        at,
    )
}

fn reply(id: &Identity, q: IdentityQuery) -> IdentityReply {
    decode_reply(&block_on(id.query(&encode_query(&q))).unwrap()).unwrap()
}

fn get(id: &Identity, number: u64) -> Option<AccountView> {
    match reply(id, IdentityQuery::Get { number }) {
        IdentityReply::Account(a) => a,
        other => panic!("expected Account, got {other:?}"),
    }
}

fn of_key(id: &Identity, key: &[u8]) -> Option<AccountView> {
    match reply(id, IdentityQuery::OfKey { key: key.to_vec() }) {
        IdentityReply::Account(a) => a,
        other => panic!("expected Account, got {other:?}"),
    }
}

fn key_gen(id: &Identity, key: &[u8]) -> u64 {
    match reply(id, IdentityQuery::KeyGen { key: key.to_vec() }) {
        IdentityReply::Gen(g) => g,
        other => panic!("expected Gen, got {other:?}"),
    }
}

fn numbers(reply: IdentityReply) -> Vec<u64> {
    match reply {
        IdentityReply::Accounts(a) => a.into_iter().map(|a| a.number).collect(),
        other => panic!("expected Accounts, got {other:?}"),
    }
}

fn all(id: &Identity, from: u64, limit: u64) -> Vec<u64> {
    numbers(reply(id, IdentityQuery::All { from, limit }))
}

fn controlled(id: &Identity, by: u64, from: u64, limit: u64) -> Vec<u64> {
    numbers(reply(id, IdentityQuery::Controlled { by, from, limit }))
}

fn control_of(id: &Identity, number: u64) -> Control {
    get(id, number).expect("account exists").control
}

fn program(controller: u64, executor: &str, generation: u64, standing: ProgramStanding) -> Control {
    Control::Program {
        controller,
        executor: executor.into(),
        generation,
        standing,
    }
}

/// the executor module every program here is provisioned by.
const AGENT: &str = "agent";

fn create_program(
    id: &mut Identity,
    executor: &str,
    name: &str,
    controller: u64,
    request: u64,
) -> Result<TestCtx, Error> {
    apply_from(
        id,
        from_module(executor),
        IdentityMsg::CreateProgram {
            name: name.into(),
            controller,
            request,
        },
    )
}

fn set_standing(
    id: &mut Identity,
    executor: &str,
    account: u64,
    standing: ProgramStanding,
) -> Result<(), Error> {
    apply_from(
        id,
        from_module(executor),
        IdentityMsg::SetProgramStanding { account, standing },
    )
    .map(|_| ())
}

fn transfer(id: &mut Identity, origin: Origin, account: u64, to: u64) -> Result<(), Error> {
    apply_from(id, origin, IdentityMsg::TransferControl { account, to }).map(|_| ())
}

fn revoke(id: &mut Identity, origin: Origin, account: u64) -> Result<(), Error> {
    apply_from(id, origin, IdentityMsg::RevokeProgram { account }).map(|_| ())
}

/// the one follow-up a provisioning op emits, authenticated the way its
/// receiver does it: by the identity module's origin.
fn program_created(ctx: &TestCtx) -> (String, IdentityEvent) {
    let [msg] = ctx.msgs() else {
        panic!("expected exactly one follow-up, got {:?}", ctx.msgs());
    };
    let event = authenticate_event(&from_module("identity"), "identity", &msg.payload)
        .expect("a genuine identity event");
    (msg.target.clone(), event)
}

fn founded(ctx: &TestCtx) -> u64 {
    match decode_assigned(ctx.assigned().expect("the op stamps")).unwrap() {
        IdentityAssigned::Founded { account } => account,
    }
}

fn refused(err: Error, needle: &str) {
    assert!(
        format!("{err:?}").contains(needle),
        "expected {needle:?}, got {err:?}"
    );
}

// ---- tests --------------------------------------------------------------

#[test]
fn create_founds_account_one_then_two() {
    let mut id = new_identity();
    let (a, b) = (ed(1), ed(2));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    create(&mut id, &ed_pub(&b), "  bob ", KeyScheme::Ed25519).unwrap();

    let alice = get(&id, 1).expect("account 1");
    assert_eq!(alice.number, 1);
    assert_eq!(alice.name, "alice");
    assert_eq!(alice.keys.len(), 1);
    assert_eq!(alice.keys[0].pubkey, ed_pub(&a));
    assert_eq!(alice.keys[0].scheme, KeyScheme::Ed25519);
    assert_eq!(get(&id, 2).unwrap().name, "bob", "names are trimmed");
    assert!(get(&id, 3).is_none());
    assert_eq!(of_key(&id, &ed_pub(&b)).unwrap().number, 2);
    assert!(of_key(&id, &ed_pub(&ed(3))).is_none());
    assert_eq!(all(&id, 0, 16), vec![1, 2], "from 0 reads from 1");
    assert_eq!(
        key_gen(&id, &ed_pub(&ed(9))),
        0,
        "a fresh key is at generation 0"
    );
}

#[test]
fn reference_resolution_is_positional_and_reads_staged_accounts() {
    let mut id = new_identity();
    let alice = ed_pub(&ed(1));
    let bob = ed_pub(&ed(2));
    create(&mut id, &alice, "alice", KeyScheme::Ed25519).unwrap();
    let mut ctx = ctx_external(&bob);
    block_on(id.execute(
        &mut ctx,
        &identity_msg(&IdentityMsg::Create {
            name: "bob".into(),
            scheme: KeyScheme::Ed25519,
        }),
    ))
    .unwrap();
    let references = vec![
        AccountRef::Key(bob.clone()),
        AccountRef::Account(1),
        AccountRef::Key(alice.clone()),
        AccountRef::Account(2),
        AccountRef::Account(0),
        AccountRef::Key(vec![99]),
        AccountRef::Account(3),
        AccountRef::Key(bob.clone()),
    ];
    assert_eq!(
        block_on(id.resolve_references(&references)).unwrap(),
        vec![
            Some(2),
            Some(1),
            Some(1),
            Some(2),
            None,
            None,
            None,
            Some(2)
        ]
    );
    block_on(id.abort_block()).unwrap();
    assert_eq!(
        block_on(id.resolve_references(&references)).unwrap(),
        vec![None, Some(1), Some(1), None, None, None, None, None]
    );
}

#[test]
fn reference_resolution_rejects_oversized_batches_and_dangling_key_indexes() {
    let mut id = new_identity();
    let alice = ed_pub(&ed(1));
    create(&mut id, &alice, "alice", KeyScheme::Ed25519).unwrap();
    let batch = vec![AccountRef::Account(1); MAX_QUERY_LIMIT as usize];
    assert_eq!(
        block_on(id.resolve_references(&batch)).unwrap(),
        vec![Some(1); batch.len()]
    );
    let oversized = vec![AccountRef::Account(1); MAX_QUERY_LIMIT as usize + 1];
    assert!(
        block_on(id.query(&encode_query(&IdentityQuery::Resolve {
            references: oversized
        })))
        .is_err()
    );
    assert!(block_on(id.resolve_references(&[])).unwrap().is_empty());
    let missing = vec![8; 32];
    id.store(key_owner_key(&missing), &99u64);
    assert!(
        block_on(id.resolve_references(&[AccountRef::Key(missing)]))
            .unwrap_err()
            .to_string()
            .contains("missing account record")
    );
    assert_eq!(
        block_on(id.resolve_references(&[AccountRef::Account(99)])).unwrap(),
        vec![None]
    );
}

#[test]
fn a_key_founds_at_most_one_account_and_names_are_not_unique() {
    let mut id = new_identity();
    let a = ed(1);
    create(&mut id, &ed_pub(&a), "eddy", KeyScheme::Ed25519).unwrap();
    refused(
        create(&mut id, &ed_pub(&a), "again", KeyScheme::Ed25519).unwrap_err(),
        "already belongs to an account",
    );
    // display-only names: a second account may pick the same one.
    create(&mut id, &ed_pub(&ed(2)), "eddy", KeyScheme::Ed25519).unwrap();
    assert_eq!(all(&id, 0, 16), vec![1, 2]);
}

#[test]
fn add_key_admits_with_member_consent_and_the_consent_is_single_use() {
    let mut id = new_identity();
    let (founder, joiner) = (ed(1), ed(2));
    create(&mut id, &ed_pub(&founder), "alice", KeyScheme::Ed25519).unwrap();

    let consent = ed_consent(&founder, KeyScheme::Ed25519, &ed_pub(&joiner), 0, 1);
    apply(
        &mut id,
        &ed_pub(&joiner),
        IdentityMsg::AddKey {
            scheme: KeyScheme::Ed25519,
            label: Some(" laptop ".into()),
            authorizer: consent.clone(),
        },
    )
    .expect("joiner admitted");
    let acc = get(&id, 1).unwrap();
    assert_eq!(acc.keys.len(), 2);
    let joined = acc
        .keys
        .iter()
        .find(|k| k.pubkey == ed_pub(&joiner))
        .unwrap();
    assert_eq!(joined.label.as_deref(), Some("laptop"));
    assert_eq!(of_key(&id, &ed_pub(&joiner)).unwrap().number, 1);
    assert_eq!(
        key_gen(&id, &ed_pub(&joiner)),
        1,
        "admission advances the generation"
    );

    // a member cannot be admitted twice ...
    refused(
        add_key(
            &mut id,
            &ed_pub(&joiner),
            KeyScheme::Ed25519,
            consent.clone(),
        )
        .unwrap_err(),
        "already belongs to an account",
    );
    // ... and the spent consent never verifies again, even for a fresh account.
    apply(
        &mut id,
        &ed_pub(&joiner),
        IdentityMsg::RemoveKey {
            key: ed_pub(&joiner),
        },
    )
    .expect("joiner leaves");
    refused(
        add_key(&mut id, &ed_pub(&joiner), KeyScheme::Ed25519, consent).unwrap_err(),
        "consent does not verify",
    );
}

#[test]
fn a_removed_key_relinks_anywhere_at_its_next_generation() {
    let mut id = new_identity();
    let (a, b, k) = (ed(1), ed(2), ed(3));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    create(&mut id, &ed_pub(&b), "bob", KeyScheme::Ed25519).unwrap();

    add_key(
        &mut id,
        &ed_pub(&k),
        KeyScheme::Ed25519,
        ed_consent(&a, KeyScheme::Ed25519, &ed_pub(&k), 0, 1),
    )
    .unwrap();
    apply(
        &mut id,
        &ed_pub(&a),
        IdentityMsg::RemoveKey { key: ed_pub(&k) },
    )
    .unwrap();
    assert!(of_key(&id, &ed_pub(&k)).is_none());
    assert_eq!(key_gen(&id, &ed_pub(&k)), 1, "removal keeps the counter");

    // bob's consent at the OLD generation is a forgery ...
    refused(
        add_key(
            &mut id,
            &ed_pub(&k),
            KeyScheme::Ed25519,
            ed_consent(&b, KeyScheme::Ed25519, &ed_pub(&k), 0, 2),
        )
        .unwrap_err(),
        "consent does not verify",
    );
    // ... at the current one it relinks the key to bob's account.
    add_key(
        &mut id,
        &ed_pub(&k),
        KeyScheme::Ed25519,
        ed_consent(&b, KeyScheme::Ed25519, &ed_pub(&k), 1, 2),
    )
    .unwrap();
    assert_eq!(of_key(&id, &ed_pub(&k)).unwrap().number, 2);
    assert_eq!(key_gen(&id, &ed_pub(&k)), 2);
    assert_eq!(get(&id, 1).unwrap().keys.len(), 1);
}

#[test]
fn a_removed_authorizer_cannot_consent() {
    let mut id = new_identity();
    let (a, b, k) = (ed(1), ed(2), ed(3));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    add_key(
        &mut id,
        &ed_pub(&b),
        KeyScheme::Ed25519,
        ed_consent(&a, KeyScheme::Ed25519, &ed_pub(&b), 0, 1),
    )
    .unwrap();
    // b mints a consent for k, then a evicts b.
    let stale = ed_consent(&b, KeyScheme::Ed25519, &ed_pub(&k), 0, 1);
    apply(
        &mut id,
        &ed_pub(&a),
        IdentityMsg::RemoveKey { key: ed_pub(&b) },
    )
    .unwrap();
    refused(
        add_key(&mut id, &ed_pub(&k), KeyScheme::Ed25519, stale).unwrap_err(),
        "authorizer belongs to no account",
    );
}

/// the consent NAMES its account, so it dies with the authorizer's membership
/// of THAT account — it never follows the authorizer onto the next one.
#[test]
fn a_consent_never_follows_its_authorizer_to_another_account() {
    let mut id = new_identity();
    let (a, b, c, k) = (ed(1), ed(2), ed(3), ed(4));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    create(&mut id, &ed_pub(&c), "carol", KeyScheme::Ed25519).unwrap();
    add_key(
        &mut id,
        &ed_pub(&b),
        KeyScheme::Ed25519,
        ed_consent(&a, KeyScheme::Ed25519, &ed_pub(&b), 0, 1),
    )
    .unwrap();

    // b mints a ticket for k on account 1 and k sits on it, unspent.
    let stale = ed_consent(&b, KeyScheme::Ed25519, &ed_pub(&k), 0, 1);
    // b leaves account 1 and relinks to carol's account 2.
    apply(
        &mut id,
        &ed_pub(&a),
        IdentityMsg::RemoveKey { key: ed_pub(&b) },
    )
    .unwrap();
    add_key(
        &mut id,
        &ed_pub(&b),
        KeyScheme::Ed25519,
        ed_consent(&c, KeyScheme::Ed25519, &ed_pub(&b), 1, 2),
    )
    .unwrap();
    assert_eq!(of_key(&id, &ed_pub(&b)).unwrap().number, 2);

    // the untouched ticket does NOT admit its holder into account 2 ...
    refused(
        add_key(&mut id, &ed_pub(&k), KeyScheme::Ed25519, stale).unwrap_err(),
        "consent names account 1, its authorizer is on account 2",
    );
    // ... and naming account 2 in the payload is not a shortcut either: the
    // signature covers the account too.
    let mut forged = ed_consent(&b, KeyScheme::Ed25519, &ed_pub(&k), 0, 1);
    forged.account = 2;
    refused(
        add_key(&mut id, &ed_pub(&k), KeyScheme::Ed25519, forged).unwrap_err(),
        "consent does not verify",
    );
    assert!(of_key(&id, &ed_pub(&k)).is_none());
}

/// an unspent consent is not a permanent bearer credential: it dies on the
/// clock, and one minted to outlive the ceiling never verifies at all.
#[test]
fn a_consent_expires_and_cannot_outlive_the_ceiling() {
    let mut id = new_identity();
    let (a, k) = (ed(1), ed(2));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();

    let consent = ed_consent_until(&a, KeyScheme::Ed25519, &ed_pub(&k), 0, 1, NOW + 10);
    refused(
        add_key_at(
            &mut id,
            &ed_pub(&k),
            KeyScheme::Ed25519,
            consent.clone(),
            NOW + 11,
        )
        .unwrap_err(),
        "consent expired at 110",
    );
    refused(
        add_key(
            &mut id,
            &ed_pub(&k),
            KeyScheme::Ed25519,
            ed_consent_until(
                &a,
                KeyScheme::Ed25519,
                &ed_pub(&k),
                0,
                1,
                NOW + MAX_CONSENT_TTL + 1,
            ),
        )
        .unwrap_err(),
        "outlives the",
    );
    // at its own consensus time it still admits.
    add_key_at(&mut id, &ed_pub(&k), KeyScheme::Ed25519, consent, NOW + 10).unwrap();
    assert_eq!(of_key(&id, &ed_pub(&k)).unwrap().number, 1);
}

/// SENIORITY: a key admitted later never evicts one admitted earlier, so a
/// redeemed stale ticket cannot take the account over. self-removal is always
/// allowed, and a senior drops a junior.
#[test]
fn a_junior_key_cannot_remove_a_senior_one() {
    let mut id = new_identity();
    let (a, b, c) = (ed(1), ed(2), ed(3));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    add_key_at(
        &mut id,
        &ed_pub(&b),
        KeyScheme::Ed25519,
        ed_consent(&a, KeyScheme::Ed25519, &ed_pub(&b), 0, 1),
        NOW + 1,
    )
    .unwrap();
    add_key_at(
        &mut id,
        &ed_pub(&c),
        KeyScheme::Ed25519,
        ed_consent(&a, KeyScheme::Ed25519, &ed_pub(&c), 0, 1),
        NOW + 2,
    )
    .unwrap();

    refused(
        apply(
            &mut id,
            &ed_pub(&c),
            IdentityMsg::RemoveKey { key: ed_pub(&a) },
        )
        .unwrap_err(),
        "admitted before your own",
    );
    refused(
        apply(
            &mut id,
            &ed_pub(&c),
            IdentityMsg::RemoveKey { key: ed_pub(&b) },
        )
        .unwrap_err(),
        "admitted before your own",
    );
    // the junior may always leave ...
    apply(
        &mut id,
        &ed_pub(&c),
        IdentityMsg::RemoveKey { key: ed_pub(&c) },
    )
    .unwrap();
    // ... and the founder drops the key it admitted.
    apply(
        &mut id,
        &ed_pub(&a),
        IdentityMsg::RemoveKey { key: ed_pub(&b) },
    )
    .unwrap();
    assert_eq!(get(&id, 1).unwrap().keys.len(), 1);
}

#[test]
fn the_last_key_is_never_removed_and_strangers_cannot_remove() {
    let mut id = new_identity();
    let a = ed(1);
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    refused(
        apply(
            &mut id,
            &ed_pub(&a),
            IdentityMsg::RemoveKey { key: ed_pub(&a) },
        )
        .unwrap_err(),
        "cannot remove the last key",
    );
    refused(
        apply(
            &mut id,
            &ed_pub(&ed(2)),
            IdentityMsg::RemoveKey { key: ed_pub(&a) },
        )
        .unwrap_err(),
        "origin key belongs to no account",
    );
    refused(
        apply(
            &mut id,
            &ed_pub(&a),
            IdentityMsg::RemoveKey {
                key: ed_pub(&ed(2)),
            },
        )
        .unwrap_err(),
        "not a member of this account",
    );
}

#[test]
fn a_wallet_founds_and_a_passkey_and_a_wallet_consent() {
    let mut id = new_identity();
    let w = wallet(3);
    create(
        &mut id,
        &wallet_pub(&w),
        "wallet-first",
        KeyScheme::Secp256k1,
    )
    .unwrap();
    assert_eq!(get(&id, 1).unwrap().keys[0].scheme, KeyScheme::Secp256k1);

    // the wallet admits a passkey ...
    let p = passkey(0x42);
    add_key(
        &mut id,
        &passkey_pub(&p),
        KeyScheme::Secp256r1,
        wallet_consent(&w, KeyScheme::Secp256r1, &passkey_pub(&p), 0, 1),
    )
    .unwrap();
    // ... the passkey admits an ed25519 device key (WebAuthn assertion consent).
    let d = ed(7);
    add_key(
        &mut id,
        &ed_pub(&d),
        KeyScheme::Ed25519,
        passkey_consent(&p, KeyScheme::Ed25519, &ed_pub(&d), 0, 1),
    )
    .unwrap();
    let acc = get(&id, 1).unwrap();
    assert_eq!(acc.keys.len(), 3);
    assert_eq!(of_key(&id, &ed_pub(&d)).unwrap().number, 1);

    // a consent minted for a different scheme of the same bytes is a forgery.
    let e = ed(8);
    refused(
        add_key(
            &mut id,
            &ed_pub(&e),
            KeyScheme::Ed25519,
            passkey_consent(&p, KeyScheme::Secp256r1, &ed_pub(&e), 0, 1),
        )
        .unwrap_err(),
        "consent does not verify",
    );
}

#[test]
fn a_malformed_key_for_its_declared_scheme_is_refused() {
    let mut id = new_identity();
    refused(
        create(&mut id, &ed_pub(&ed(1)), "x", KeyScheme::Secp256k1).unwrap_err(),
        "malformed for its scheme",
    );
    refused(
        create(&mut id, &[7u8; 5], "x", KeyScheme::Ed25519).unwrap_err(),
        "malformed for its scheme",
    );
    create(&mut id, &ed_pub(&ed(1)), "x", KeyScheme::Ed25519).unwrap();
    refused(
        add_key(
            &mut id,
            &[7u8; 5],
            KeyScheme::Ed25519,
            ed_consent(&ed(1), KeyScheme::Ed25519, &[7u8; 5], 0, 1),
        )
        .unwrap_err(),
        "malformed for its scheme",
    );
}

/// a secp key has ONE spelling here. the uncompressed SEC1 form of a wallet's
/// key is the same curve point — every consent over it verifies — but the key
/// index is raw bytes, so admitting it would found a second account for one
/// private key, and let a member re-join under a spelling `RemoveKey` on the
/// compressed form does not touch.
#[test]
fn the_uncompressed_spelling_of_a_secp_key_is_refused() {
    let mut id = new_identity();
    let w = wallet(3);
    let uncompressed = w
        .verifying_key()
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();
    assert_eq!(uncompressed.len(), 65);
    refused(
        create(&mut id, &uncompressed, "wallet", KeyScheme::Secp256k1).unwrap_err(),
        "malformed for its scheme",
    );

    // and it can never be consented INTO the account its compressed form owns.
    create(&mut id, &wallet_pub(&w), "wallet", KeyScheme::Secp256k1).unwrap();
    refused(
        add_key(
            &mut id,
            &uncompressed,
            KeyScheme::Secp256k1,
            wallet_consent(&w, KeyScheme::Secp256k1, &uncompressed, 0, 1),
        )
        .unwrap_err(),
        "malformed for its scheme",
    );
    assert_eq!(get(&id, 1).unwrap().keys.len(), 1);
}

#[test]
fn set_name_and_profile_are_member_gated_trimmed_and_capped() {
    let mut id = new_identity();
    let a = ed(1);
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    refused(
        apply(
            &mut id,
            &ed_pub(&ed(2)),
            IdentityMsg::SetName { name: "x".into() },
        )
        .unwrap_err(),
        "origin key belongs to no account",
    );
    refused(
        apply(
            &mut id,
            &ed_pub(&a),
            IdentityMsg::SetName { name: "   ".into() },
        )
        .unwrap_err(),
        "account name is empty",
    );
    refused(
        apply(
            &mut id,
            &ed_pub(&a),
            IdentityMsg::SetName {
                name: "n".repeat(MAX_NAME_LEN + 1),
            },
        )
        .unwrap_err(),
        "exceeds",
    );
    refused(
        create(&mut id, &ed_pub(&ed(3)), "", KeyScheme::Ed25519).unwrap_err(),
        "account name is empty",
    );
    apply(
        &mut id,
        &ed_pub(&a),
        IdentityMsg::SetName {
            name: " Alice ".into(),
        },
    )
    .unwrap();
    apply(
        &mut id,
        &ed_pub(&a),
        IdentityMsg::SetProfile {
            avatar: Some("/shared/attachments/avatars/cafe.png".into()),
            bio: Some("  ".into()),
        },
    )
    .unwrap();
    let acc = get(&id, 1).unwrap();
    assert_eq!(acc.name, "Alice");
    assert_eq!(
        acc.avatar.as_deref(),
        Some("/shared/attachments/avatars/cafe.png")
    );
    assert_eq!(acc.bio, None, "empty trims clear");
    refused(
        apply(
            &mut id,
            &ed_pub(&a),
            IdentityMsg::SetProfile {
                avatar: None,
                bio: Some("b".repeat(MAX_BIO_LEN + 1)),
            },
        )
        .unwrap_err(),
        "bio exceeds",
    );
}

#[test]
fn abort_block_drops_staged_accounts() {
    let mut id = new_identity();
    let a = ed(1);
    let mut ctx = ctx_external(&ed_pub(&a));
    let m = Msg {
        target: "identity".into(),
        payload: encode_msg(&IdentityMsg::Create {
            name: "alice".into(),
            scheme: KeyScheme::Ed25519,
        }),
    };
    block_on(id.execute(&mut ctx, &m)).unwrap();
    assert!(get(&id, 1).is_some(), "staged writes are read-your-writes");
    block_on(id.abort_block()).unwrap();
    assert!(get(&id, 1).is_none());
    assert!(of_key(&id, &ed_pub(&a)).is_none());
    assert_eq!(all(&id, 0, 16), Vec::<u64>::new());
}

#[test]
fn all_pages_by_number() {
    let mut id = new_identity();
    for seed in 1..=3 {
        create(&mut id, &ed_pub(&ed(seed)), "n", KeyScheme::Ed25519).unwrap();
    }
    assert_eq!(all(&id, 2, 1), vec![2]);
    assert_eq!(all(&id, 2, 16), vec![2, 3]);
    assert_eq!(all(&id, 0, 0), Vec::<u64>::new());
    assert_eq!(all(&id, 9, 16), Vec::<u64>::new());
}

// ---- program accounts --------------------------------------------------

#[test]
fn create_program_founds_a_keyless_account_and_tells_the_executor() {
    let mut id = new_identity();
    let a = ed(1);
    let founding = apply_from(
        &mut id,
        from_key(&ed_pub(&a)),
        IdentityMsg::Create {
            name: "alice".into(),
            scheme: KeyScheme::Ed25519,
        },
    )
    .unwrap();
    assert_eq!(
        founded(&founding),
        1,
        "a key-held founding stamps its number"
    );

    let ctx = create_program(&mut id, AGENT, " bot ", 1, 77).unwrap();
    let bot = get(&id, 2).expect("account 2");
    assert_eq!(bot.name, "bot", "names are trimmed");
    assert_eq!(bot.control, program(1, AGENT, 0, ProgramStanding::Active));
    assert!(bot.keys.is_empty(), "a program holds no key");
    assert_eq!(bot.updated_at, NOW);
    assert_eq!(founded(&ctx), 2);
    // the follow-up goes to the executor, in this unit, authenticated by
    // identity's own origin, echoing the executor's correlation.
    let (target, event) = program_created(&ctx);
    assert_eq!(target, AGENT);
    assert_eq!(
        event,
        IdentityEvent::ProgramCreated {
            request: 77,
            account: 2,
            controller: 1,
        }
    );
    // the program shares the numbering and the listing, and is invisible to
    // the key resolver.
    assert_eq!(all(&id, 0, 16), vec![1, 2]);
    assert_eq!(controlled(&id, 1, 0, 16), vec![2]);
    assert_eq!(of_key(&id, &ed_pub(&a)).unwrap().number, 1);
    assert_eq!(control_of(&id, 1), Control::Keys);
    assert_eq!(
        key_gen(&id, &2u64.to_le_bytes()),
        0,
        "an account number is not a key"
    );
    assert!(of_key(&id, &2u64.to_le_bytes()).is_none());
}

#[test]
fn create_program_is_module_origin_only() {
    let mut id = new_identity();
    let a = ed(1);
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    let request = IdentityMsg::CreateProgram {
        name: "bot".into(),
        controller: 1,
        request: 1,
    };
    for origin in [from_key(&ed_pub(&a)), as_program(1), Origin::System] {
        refused(
            apply_from(&mut id, origin.clone(), request.clone()).unwrap_err(),
            "module-origin only",
        );
    }
    assert_eq!(all(&id, 0, 16), vec![1], "nothing was founded");
    assert_eq!(controlled(&id, 1, 0, 16), Vec::<u64>::new());
}

#[test]
fn create_program_requires_a_live_controller() {
    let mut id = new_identity();
    let a = ed(1);
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    refused(
        create_program(&mut id, AGENT, "bot", 0, 1).unwrap_err(),
        "controller 0 is not an account",
    );
    refused(
        create_program(&mut id, AGENT, "bot", 9, 1).unwrap_err(),
        "controller 9 is not an account",
    );
    // an active program controls: a program provisions its own sub-program.
    create_program(&mut id, AGENT, "bot", 1, 1).unwrap();
    create_program(&mut id, AGENT, "sub-bot", 2, 2).unwrap();
    assert_eq!(
        control_of(&id, 3),
        program(2, AGENT, 0, ProgramStanding::Active)
    );
    assert_eq!(controlled(&id, 2, 0, 16), vec![3]);
    // a suspended one does not ...
    set_standing(&mut id, AGENT, 2, ProgramStanding::Suspended).unwrap();
    refused(
        create_program(&mut id, AGENT, "late", 2, 3).unwrap_err(),
        "controller 2 cannot control",
    );
    // ... nor a revoked one.
    revoke(&mut id, from_key(&ed_pub(&a)), 2).unwrap();
    refused(
        create_program(&mut id, AGENT, "late", 2, 3).unwrap_err(),
        "controller 2 cannot control",
    );
    refused(
        create_program(&mut id, AGENT, "  ", 1, 3).unwrap_err(),
        "account name is empty",
    );
    assert_eq!(
        all(&id, 0, 16),
        vec![1, 2, 3],
        "every refusal founded nothing"
    );
    assert_eq!(controlled(&id, 2, 0, 16), vec![3]);
}

/// key ops never see a program: its origin manages no key, and no consent can
/// name it, because a consent is signed by a member key and it has none.
#[test]
fn program_accounts_never_hold_keys() {
    let mut id = new_identity();
    let (a, k) = (ed(1), ed(2));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    create_program(&mut id, AGENT, "bot", 1, 1).unwrap();

    refused(
        apply_from(
            &mut id,
            as_program(2),
            IdentityMsg::Create {
                name: "again".into(),
                scheme: KeyScheme::Ed25519,
            },
        )
        .unwrap_err(),
        "a program account holds no key",
    );
    refused(
        apply_from(
            &mut id,
            as_program(2),
            IdentityMsg::AddKey {
                scheme: KeyScheme::Ed25519,
                label: None,
                authorizer: ed_consent(&a, KeyScheme::Ed25519, &ed_pub(&k), 0, 1),
            },
        )
        .unwrap_err(),
        "a program account holds no key",
    );
    refused(
        apply_from(
            &mut id,
            as_program(2),
            IdentityMsg::RemoveKey { key: ed_pub(&a) },
        )
        .unwrap_err(),
        "a program account holds no key",
    );
    // a consent naming the program admits nothing: the account it names is
    // not the authorizer's, and the program has no member to sign one.
    let mut forged = ed_consent(&a, KeyScheme::Ed25519, &ed_pub(&k), 0, 2);
    forged.account = 2;
    refused(
        add_key(&mut id, &ed_pub(&k), KeyScheme::Ed25519, forged).unwrap_err(),
        "consent names account 2, its authorizer is on account 1",
    );
    assert!(get(&id, 2).unwrap().keys.is_empty());
    assert_eq!(get(&id, 1).unwrap().keys.len(), 1);
    assert!(of_key(&id, &ed_pub(&k)).is_none());
}

/// the host runs a program's call as `Origin::Program(account)`; here that
/// origin acts as its account for what a member key does to its own account,
/// and is refused whenever the record says the host should not have run it.
#[test]
fn a_program_origin_acts_as_its_account_for_name_and_profile() {
    let mut id = new_identity();
    let a = ed(1);
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    create_program(&mut id, AGENT, "bot", 1, 1).unwrap();

    apply_from(
        &mut id,
        as_program(2),
        IdentityMsg::SetName {
            name: "bot-2".into(),
        },
    )
    .unwrap();
    apply_from(
        &mut id,
        as_program(2),
        IdentityMsg::SetProfile {
            avatar: None,
            bio: Some("beep".into()),
        },
    )
    .unwrap();
    let bot = get(&id, 2).unwrap();
    assert_eq!(bot.name, "bot-2");
    assert_eq!(bot.bio.as_deref(), Some("beep"));
    assert_eq!(get(&id, 1).unwrap().name, "alice", "only its own account");

    let rename = IdentityMsg::SetName { name: "x".into() };
    refused(
        apply_from(&mut id, as_program(1), rename.clone()).unwrap_err(),
        "account 1, which is not an active program",
    );
    refused(
        apply_from(&mut id, as_program(9), rename.clone()).unwrap_err(),
        "account 9, which does not exist",
    );
    refused(
        apply_from(&mut id, from_module(AGENT), rename.clone()).unwrap_err(),
        "a module or the system holds none",
    );
    set_standing(&mut id, AGENT, 2, ProgramStanding::Suspended).unwrap();
    refused(
        apply_from(&mut id, as_program(2), rename.clone()).unwrap_err(),
        "account 2, which is not an active program",
    );
    set_standing(&mut id, AGENT, 2, ProgramStanding::Active).unwrap();
    apply_from(&mut id, as_program(2), rename.clone()).unwrap();
    revoke(&mut id, from_key(&ed_pub(&a)), 2).unwrap();
    refused(
        apply_from(&mut id, as_program(2), rename).unwrap_err(),
        "account 2, which is not an active program",
    );
}

#[test]
fn set_program_standing_is_executor_only_and_advances_the_generation() {
    let mut id = new_identity();
    let a = ed(1);
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    create_program(&mut id, AGENT, "bot", 1, 1).unwrap();

    refused(
        set_standing(&mut id, "chat", 2, ProgramStanding::Suspended).unwrap_err(),
        "only its executor",
    );
    refused(
        apply_from(
            &mut id,
            from_key(&ed_pub(&a)),
            IdentityMsg::SetProgramStanding {
                account: 2,
                standing: ProgramStanding::Suspended,
            },
        )
        .unwrap_err(),
        "module-origin only",
    );
    refused(
        set_standing(&mut id, AGENT, 1, ProgramStanding::Suspended).unwrap_err(),
        "key-held, not a program",
    );
    refused(
        set_standing(&mut id, AGENT, 9, ProgramStanding::Suspended).unwrap_err(),
        "account 9 does not exist",
    );
    assert_eq!(
        control_of(&id, 2),
        program(1, AGENT, 0, ProgramStanding::Active)
    );

    // every call advances the generation, a repeated standing included: the
    // op's promise is that nothing queued before it stays executable.
    set_standing(&mut id, AGENT, 2, ProgramStanding::Suspended).unwrap();
    assert_eq!(
        control_of(&id, 2),
        program(1, AGENT, 1, ProgramStanding::Suspended)
    );
    set_standing(&mut id, AGENT, 2, ProgramStanding::Active).unwrap();
    assert_eq!(
        control_of(&id, 2),
        program(1, AGENT, 2, ProgramStanding::Active)
    );
    set_standing(&mut id, AGENT, 2, ProgramStanding::Active).unwrap();
    assert_eq!(
        control_of(&id, 2),
        program(1, AGENT, 3, ProgramStanding::Active)
    );
}

/// an exhausted generation cannot advance, so the record must not change at
/// all: every queued call saw it exactly as it is, and a wrap would revive
/// one. revocation needs no generation and still applies.
#[test]
fn an_exhausted_generation_refuses_every_mutation_before_any_write() {
    let mut id = new_identity();
    let (a, b) = (ed(1), ed(2));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    create(&mut id, &ed_pub(&b), "bob", KeyScheme::Ed25519).unwrap();
    create_program(&mut id, AGENT, "bot", 1, 1).unwrap();
    // stage the record at the last generation, the way 2^64 - 1 mutations
    // would have.
    let exhausted = AccountRecord {
        name: "bot".into(),
        control: ControlRecord::Program(ProgramControl {
            controller: 1,
            executor: AGENT.into(),
            generation: u64::MAX,
            standing: ProgramStanding::Active,
        }),
        avatar: None,
        bio: None,
        updated_at: NOW,
    };
    id.store_account(3, &exhausted).unwrap();
    block_on(id.commit_block()).unwrap();
    let before = get(&id, 3).unwrap();

    refused(
        set_standing(&mut id, AGENT, 3, ProgramStanding::Suspended).unwrap_err(),
        "generation is exhausted",
    );
    refused(
        transfer(&mut id, from_key(&ed_pub(&a)), 3, 2).unwrap_err(),
        "generation is exhausted",
    );
    assert_eq!(get(&id, 3).unwrap(), before, "the record did not move");
    assert_eq!(controlled(&id, 1, 0, 16), vec![3]);
    assert_eq!(controlled(&id, 2, 0, 16), Vec::<u64>::new());

    revoke(&mut id, from_key(&ed_pub(&a)), 3).unwrap();
    assert_eq!(control_of(&id, 3), Control::Revoked { controller: 1 });
}

#[test]
fn transfer_control_is_controller_only_and_never_cycles() {
    let mut id = new_identity();
    let (a, b) = (ed(1), ed(2));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    create(&mut id, &ed_pub(&b), "bob", KeyScheme::Ed25519).unwrap();
    // 3 is alice's; 4 is 3's; 5 is bob's and suspended.
    create_program(&mut id, AGENT, "p", 1, 1).unwrap();
    create_program(&mut id, AGENT, "q", 3, 2).unwrap();
    create_program(&mut id, AGENT, "r", 2, 3).unwrap();
    set_standing(&mut id, AGENT, 5, ProgramStanding::Suspended).unwrap();

    let alice = || from_key(&ed_pub(&a));
    let bob = || from_key(&ed_pub(&b));
    refused(
        transfer(&mut id, bob(), 3, 2).unwrap_err(),
        "only its controller transfers control",
    );
    refused(
        transfer(&mut id, from_module(AGENT), 3, 2).unwrap_err(),
        "a module or the system holds none",
    );
    refused(
        transfer(&mut id, alice(), 3, 3).unwrap_err(),
        "an account cannot control itself",
    );
    refused(
        transfer(&mut id, alice(), 3, 4).unwrap_err(),
        "account 4 is controlled by 3: control would cycle",
    );
    refused(
        transfer(&mut id, alice(), 3, 1).unwrap_err(),
        "account 3 is already controlled by 1",
    );
    refused(
        transfer(&mut id, alice(), 3, 9).unwrap_err(),
        "account 9 does not exist",
    );
    refused(
        transfer(&mut id, alice(), 3, 5).unwrap_err(),
        "account 5 cannot control",
    );
    refused(
        transfer(&mut id, alice(), 1, 2).unwrap_err(),
        "key-held, not a program",
    );
    refused(
        transfer(&mut id, alice(), 9, 2).unwrap_err(),
        "account 9 does not exist",
    );
    // every refusal left the forest and the generation as they were.
    assert_eq!(
        control_of(&id, 3),
        program(1, AGENT, 0, ProgramStanding::Active)
    );
    assert_eq!(controlled(&id, 1, 0, 16), vec![3]);
    assert_eq!(controlled(&id, 2, 0, 16), vec![5]);
    assert_eq!(controlled(&id, 3, 0, 16), vec![4]);

    // alice hands 3 to bob: the generation advances, the sets move, and the
    // old controller is a stranger from here on.
    transfer(&mut id, alice(), 3, 2).unwrap();
    assert_eq!(
        control_of(&id, 3),
        program(2, AGENT, 1, ProgramStanding::Active)
    );
    assert_eq!(controlled(&id, 1, 0, 16), Vec::<u64>::new());
    assert_eq!(controlled(&id, 2, 0, 16), vec![3, 5]);
    refused(
        revoke(&mut id, alice(), 3).unwrap_err(),
        "only its controller revokes",
    );
    // a deeper cycle: 4 -> 3 -> 2, so 2's program 3 can never go to 4.
    refused(
        transfer(&mut id, bob(), 3, 4).unwrap_err(),
        "control would cycle",
    );
    // a program acts as controller: 3 hands its sub-program 4 to bob.
    transfer(&mut id, as_program(3), 4, 2).unwrap();
    assert_eq!(
        control_of(&id, 4),
        program(2, AGENT, 1, ProgramStanding::Active)
    );
    assert_eq!(controlled(&id, 3, 0, 16), Vec::<u64>::new());
    assert_eq!(controlled(&id, 2, 0, 16), vec![3, 4, 5]);
    // standing survives a transfer untouched.
    transfer(&mut id, bob(), 5, 1).unwrap();
    assert_eq!(
        control_of(&id, 5),
        program(1, AGENT, 2, ProgramStanding::Suspended)
    );
}

#[test]
fn revoke_program_freezes_the_record_for_good() {
    let mut id = new_identity();
    let (a, b) = (ed(1), ed(2));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    create_program(&mut id, AGENT, "bot", 1, 1).unwrap();
    create(&mut id, &ed_pub(&b), "bob", KeyScheme::Ed25519).unwrap();
    set_standing(&mut id, AGENT, 2, ProgramStanding::Suspended).unwrap();

    refused(
        revoke(&mut id, from_key(&ed_pub(&b)), 2).unwrap_err(),
        "only its controller revokes",
    );
    refused(
        revoke(&mut id, from_module(AGENT), 2).unwrap_err(),
        "a module or the system holds none",
    );
    refused(
        revoke(&mut id, from_key(&ed_pub(&a)), 1).unwrap_err(),
        "key-held, not a program",
    );
    revoke(&mut id, from_key(&ed_pub(&a)), 2).unwrap();
    let bot = get(&id, 2).unwrap();
    assert_eq!(bot.control, Control::Revoked { controller: 1 });
    assert!(bot.keys.is_empty());
    assert_eq!(bot.name, "bot", "the profile stays readable");

    // nothing touches it again.
    refused(
        revoke(&mut id, from_key(&ed_pub(&a)), 2).unwrap_err(),
        "program is revoked",
    );
    refused(
        set_standing(&mut id, AGENT, 2, ProgramStanding::Active).unwrap_err(),
        "program is revoked",
    );
    refused(
        transfer(&mut id, from_key(&ed_pub(&a)), 2, 3).unwrap_err(),
        "program is revoked",
    );
    assert_eq!(control_of(&id, 2), Control::Revoked { controller: 1 });
    // it still reads as the controller's, and it can never control.
    assert_eq!(controlled(&id, 1, 0, 16), vec![2]);
    refused(
        create_program(&mut id, AGENT, "orphan", 2, 5).unwrap_err(),
        "controller 2 cannot control",
    );
}

#[test]
fn controlled_pages_by_number_under_the_query_clamp() {
    let mut id = new_identity();
    let (a, b) = (ed(1), ed(2));
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    for request in 1..=3 {
        create_program(&mut id, AGENT, "bot", 1, request).unwrap();
    }
    create(&mut id, &ed_pub(&b), "bob", KeyScheme::Ed25519).unwrap();
    create_program(&mut id, AGENT, "bot", 5, 4).unwrap();

    assert_eq!(
        controlled(&id, 1, 0, 16),
        vec![2, 3, 4],
        "from 0 reads from 1"
    );
    assert_eq!(controlled(&id, 1, 3, 16), vec![3, 4]);
    assert_eq!(controlled(&id, 1, 0, 2), vec![2, 3]);
    assert_eq!(controlled(&id, 1, 9, 16), Vec::<u64>::new());
    assert_eq!(controlled(&id, 1, 0, 0), Vec::<u64>::new());
    assert_eq!(
        controlled(&id, 1, 0, u64::MAX),
        vec![2, 3, 4],
        "the clamp is a page bound"
    );
    assert_eq!(controlled(&id, 5, 0, 16), vec![6]);
    assert_eq!(controlled(&id, 9, 0, 16), Vec::<u64>::new());
    assert_eq!(
        controlled(&id, 2, 0, 16),
        Vec::<u64>::new(),
        "a program with no programs"
    );
    assert_eq!(all(&id, 0, 16), vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn abort_block_drops_a_staged_program_and_its_follow_up_never_lands() {
    let mut id = new_identity();
    let a = ed(1);
    create(&mut id, &ed_pub(&a), "alice", KeyScheme::Ed25519).unwrap();
    let mut ctx = ctx_with(from_module(AGENT), NOW);
    let request = IdentityMsg::CreateProgram {
        name: "bot".into(),
        controller: 1,
        request: 1,
    };
    block_on(id.execute(&mut ctx, &identity_msg(&request))).unwrap();
    assert!(get(&id, 2).is_some(), "staged writes are read-your-writes");
    assert_eq!(controlled(&id, 1, 0, 16), vec![2]);
    assert_eq!(
        program_created(&ctx).1,
        IdentityEvent::ProgramCreated {
            request: 1,
            account: 2,
            controller: 1,
        }
    );
    block_on(id.abort_block()).unwrap();
    assert!(get(&id, 2).is_none());
    assert_eq!(controlled(&id, 1, 0, 16), Vec::<u64>::new());
    assert_eq!(all(&id, 0, 16), vec![1]);
    // the numbering did not move: the next program takes 2.
    create_program(&mut id, AGENT, "bot", 1, 2).unwrap();
    assert_eq!(all(&id, 0, 16), vec![1, 2]);
}
