use std::collections::{BTreeMap, BTreeSet};

use commonware_cryptography::{Signer as _, ed25519};
use futures::executor::block_on;
use gateway::identity_contract as identity;
use gateway::identity_contract::{AccountView, IdentityQuery, IdentityReply, KeyView};
use gateway::{
    CredentialGrantStatement, CredentialKind, CredentialRecord, DuckDnsName, GATEWAY_CREDENTIAL_NS,
    GATEWAY_ROUTE_NS, Gateway, GatewayMsg, GatewayQuery, GatewayReply, MemberAuthorization,
    RemoveCredentialStatement, ResolvedAccount, RouteAudience, RouteDefinition, RouteMethod,
    RouteName, RoutePolicy, RouteStatement, RouteTarget, SetCredentialStatement,
    credential_use_allowed, decode_msg, decode_reply, encode_msg, encode_query,
    grant_credential_preimage, remove_credential_preimage, revoke_credential_preimage,
    route_signing_preimage, set_credential_preimage, validate_credential_name,
};
use keyscheme::KeyScheme;
use sdk::{Ctx, Env, Error, Event, Module, Msg, Origin, StateRoot};

const CHAIN_ID: &str = "test#12345678";

/// the node every route names as its publisher: a plain 32-byte node key the
/// signing account vouches for, bound to no account.
fn node() -> Vec<u8> {
    vec![1; 32]
}

/// the identity double: accounts keyed by ORIGIN KEY, answering `OfKey`.
struct TestCtx {
    env: Env,
    accounts: BTreeMap<Vec<u8>, AccountView>,
}

impl TestCtx {
    fn new(origin: Vec<u8>, accounts: BTreeMap<Vec<u8>, AccountView>) -> Self {
        Self {
            env: Env {
                height: 1,
                consensus_time: 1,
                origin: Origin::External(origin),
                me: "gateway".into(),
                cause: sdk::Cause::Direct,
            },
            accounts,
        }
    }

    fn act_as(&mut self, key: Vec<u8>) {
        self.env.origin = Origin::External(key);
    }
}

#[async_trait::async_trait(?Send)]
impl Ctx for TestCtx {
    fn env(&self) -> &Env {
        &self.env
    }

    fn module_root(&self, _target: &str) -> Option<StateRoot> {
        None
    }

    async fn query(&self, target: &str, request: &[u8]) -> Result<Vec<u8>, Error> {
        match target {
            "identity" => {
                let IdentityQuery::OfKey { key } =
                    identity::decode_query(request).map_err(|e| Error::module("codec", e))?;
                Ok(identity::encode_reply(&IdentityReply::Account(
                    self.accounts.get(&key).cloned(),
                )))
            }
            _ => Err(Error::UnknownModule(target.into())),
        }
    }

    fn emit_msg(&mut self, _msg: Msg) {}
    fn emit_event(&mut self, _event: Event) {}
}

fn pubkey(signer: &ed25519::PrivateKey) -> Vec<u8> {
    signer.public_key().as_ref().to_vec()
}

fn account(number: u64, signer: &ed25519::PrivateKey) -> AccountView {
    AccountView {
        control: identity::Control::Keys,
        number,
        name: "Alice".into(),
        keys: vec![KeyView {
            scheme: KeyScheme::Ed25519,
            pubkey: pubkey(signer),
            label: None,
            added_at: 0,
        }],
        avatar: None,
        bio: None,
        updated_at: 0,
    }
}

fn statement(account_id: u64, node: Vec<u8>, name: RouteName, revision: u64) -> RouteStatement {
    RouteStatement {
        chain_id: CHAIN_ID.into(),
        account_id,
        name,
        publisher_node: node,
        revision,
        route: Some(RouteDefinition {
            target: RouteTarget::LoopbackHttp,
            policy: RoutePolicy {
                audience: RouteAudience::Network,
                methods: vec![RouteMethod::Get, RouteMethod::Post],
                max_request_bytes: Some(4096),
                max_response_bytes: 8192,
                allow_authorization: false,
                allow_upgrade: false,
            },
        }),
    }
}

fn signed(statement: RouteStatement, signer: &ed25519::PrivateKey) -> GatewayMsg {
    let signature = signer
        .sign(
            GATEWAY_ROUTE_NS,
            &route_signing_preimage(&statement).unwrap(),
        )
        .as_ref()
        .to_vec();
    GatewayMsg::SetRoute {
        statement,
        authorization: MemberAuthorization {
            signer: pubkey(signer),
            signature,
        },
    }
}

fn execute(module: &mut Gateway, ctx: &mut TestCtx, message: GatewayMsg) -> Result<(), Error> {
    block_on(module.execute(
        ctx,
        &Msg {
            target: "gateway".into(),
            payload: encode_msg(&message),
        },
    ))
}

fn gateway() -> Gateway {
    Gateway::new(
        "gateway",
        Box::new(sdk_testkit::MemStore::new()),
        "identity",
        CHAIN_ID,
    )
}

/// account 1, owned by the seed's key, acting as the frame origin.
fn fixture(seed: u64) -> (u64, ed25519::PrivateKey, AccountView, TestCtx, Gateway) {
    let signer = ed25519::PrivateKey::from_seed(seed);
    let account = account(1, &signer);
    let context = TestCtx::new(
        pubkey(&signer),
        BTreeMap::from([(pubkey(&signer), account.clone())]),
    );
    (account.number, signer, account, context, gateway())
}

#[test]
fn route_requires_an_account_origin_current_member_and_valid_signature() {
    let (number, signer, _account, mut context, mut module) = fixture(7);
    let publish = signed(
        statement(number, node(), RouteName::named("api"), 1),
        &signer,
    );
    execute(&mut module, &mut context, publish.clone()).unwrap();
    block_on(module.commit_block()).unwrap();

    let reply = block_on(module.query(&encode_query(&GatewayQuery::Get {
        account_id: number,
        name: RouteName::named("api"),
    })))
    .unwrap();
    assert!(matches!(
        decode_reply(&reply).unwrap(),
        GatewayReply::Route(route) if route.is_some()
    ));

    // a key of no account cannot act at all.
    let stranger = ed25519::PrivateKey::from_seed(70);
    context.act_as(pubkey(&stranger));
    assert!(
        execute(&mut module, &mut context, publish.clone())
            .unwrap_err()
            .to_string()
            .contains("no Identity account")
    );

    // another account's member cannot publish under this account.
    let other = ed25519::PrivateKey::from_seed(71);
    context.accounts.insert(pubkey(&other), account(2, &other));
    context.act_as(pubkey(&other));
    assert!(
        execute(&mut module, &mut context, publish.clone())
            .unwrap_err()
            .to_string()
            .contains("not the origin's account")
    );

    // the origin's account, but signed by a key that is not a member of it.
    context.act_as(pubkey(&signer));
    assert!(
        execute(
            &mut module,
            &mut context,
            signed(
                statement(number, node(), RouteName::named("api"), 2),
                &other
            )
        )
        .unwrap_err()
        .to_string()
        .contains("not a current account member")
    );

    let mut forged = publish;
    let GatewayMsg::SetRoute { authorization, .. } = &mut forged else {
        unreachable!("publish is a SetRoute");
    };
    authorization.signature[0] ^= 1;
    assert!(
        execute(&mut module, &mut context, forged)
            .unwrap_err()
            .to_string()
            .contains("verify")
    );
}

#[test]
fn apex_and_named_routes_share_one_authority_model_but_independent_revisions() {
    let (number, signer, _account, mut context, mut module) = fixture(8);
    for name in [RouteName::apex(), RouteName::named("api")] {
        execute(
            &mut module,
            &mut context,
            signed(statement(number, node(), name, 1), &signer),
        )
        .unwrap();
    }
    block_on(module.commit_block()).unwrap();

    for name in [RouteName::apex(), RouteName::named("api")] {
        let reply = block_on(module.query(&encode_query(&GatewayQuery::Get {
            account_id: number,
            name,
        })))
        .unwrap();
        assert!(matches!(
            decode_reply(&reply).unwrap(),
            GatewayReply::Route(route) if route.is_some()
        ));
    }

    let reply =
        block_on(module.query(&encode_query(&GatewayQuery::List { account_id: number }))).unwrap();
    let GatewayReply::Routes(routes) = decode_reply(&reply).unwrap() else {
        panic!("list must return routes");
    };
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].name, RouteName::apex());
    assert_eq!(routes[1].name, RouteName::named("api"));
    assert_eq!(routes[0].revision, 1);
}

#[test]
fn handle_plane_resolves_to_the_origin_account_and_shares_the_route_gates() {
    // the merged module owns the `.duck` handle plane too: an account member
    // registers a name that resolves to the account NUMBER, and a key of no
    // account is refused by the SAME identity gate the route plane uses.
    let (number, _signer, _account, mut context, mut module) = fixture(21);
    execute(
        &mut module,
        &mut context,
        GatewayMsg::SetHandle {
            handle: Some("orthory".into()),
        },
    )
    .unwrap();
    block_on(module.commit_block()).unwrap();

    let reply = block_on(module.query(&encode_query(&GatewayQuery::Resolve {
        name: DuckDnsName {
            handle: "orthory".into(),
        },
    })))
    .unwrap();
    assert_eq!(
        decode_reply(&reply).unwrap(),
        GatewayReply::Resolved(Some(ResolvedAccount { account_id: number }))
    );

    // a reserved root label is refused at admission.
    assert!(
        execute(
            &mut module,
            &mut context,
            GatewayMsg::SetHandle {
                handle: Some("net".into()),
            },
        )
        .unwrap_err()
        .to_string()
        .contains("reserved")
    );

    // the account gate is shared: a key of no account cannot set a handle.
    context.act_as(vec![2; 32]);
    assert!(
        execute(
            &mut module,
            &mut context,
            GatewayMsg::SetHandle {
                handle: Some("intruder".into()),
            },
        )
        .unwrap_err()
        .to_string()
        .contains("no Identity account")
    );
}

#[test]
fn route_revision_and_chain_prevent_replay() {
    let (number, signer, _account, mut context, mut module) = fixture(9);
    let first = signed(statement(number, node(), RouteName::apex(), 1), &signer);
    execute(&mut module, &mut context, first.clone()).unwrap();
    block_on(module.commit_block()).unwrap();
    assert!(
        execute(&mut module, &mut context, first)
            .unwrap_err()
            .to_string()
            .contains("revision")
    );

    let mut wrong_chain = statement(number, node(), RouteName::apex(), 2);
    wrong_chain.chain_id = "other#12345678".into();
    assert!(
        execute(&mut module, &mut context, signed(wrong_chain, &signer))
            .unwrap_err()
            .to_string()
            .contains("another chain")
    );
}

fn credential(name: &str, owner_account: u64, publisher_node: Vec<u8>) -> CredentialRecord {
    CredentialRecord {
        name: name.into(),
        owner_account,
        publisher_node,
        kind: CredentialKind::Claude,
        seal_pk: [9; 32],
        grants: BTreeSet::new(),
    }
}

fn signed_set_credential(signer: &ed25519::PrivateKey, record: CredentialRecord) -> GatewayMsg {
    let statement = SetCredentialStatement {
        chain_id: CHAIN_ID.into(),
        record,
    };
    let preimage = set_credential_preimage(&statement).unwrap();
    let signature = signer
        .sign(GATEWAY_CREDENTIAL_NS, &preimage)
        .as_ref()
        .to_vec();
    GatewayMsg::SetCredential {
        statement,
        authorization: MemberAuthorization {
            signer: pubkey(signer),
            signature,
        },
    }
}

fn signed_remove(signer: &ed25519::PrivateKey, owner_account: u64, name: &str) -> GatewayMsg {
    let statement = RemoveCredentialStatement {
        chain_id: CHAIN_ID.into(),
        owner_account,
        name: name.into(),
    };
    let preimage = remove_credential_preimage(&statement).unwrap();
    let signature = signer
        .sign(GATEWAY_CREDENTIAL_NS, &preimage)
        .as_ref()
        .to_vec();
    GatewayMsg::RemoveCredential {
        statement,
        authorization: MemberAuthorization {
            signer: pubkey(signer),
            signature,
        },
    }
}

fn grant_statement(owner_account: u64, name: &str, account: u64) -> CredentialGrantStatement {
    CredentialGrantStatement {
        chain_id: CHAIN_ID.into(),
        owner_account,
        name: name.into(),
        account,
    }
}

fn signed_grant(
    signer: &ed25519::PrivateKey,
    owner_account: u64,
    name: &str,
    account: u64,
) -> GatewayMsg {
    let statement = grant_statement(owner_account, name, account);
    let preimage = grant_credential_preimage(&statement).unwrap();
    let signature = signer
        .sign(GATEWAY_CREDENTIAL_NS, &preimage)
        .as_ref()
        .to_vec();
    GatewayMsg::GrantCredential {
        statement,
        authorization: MemberAuthorization {
            signer: pubkey(signer),
            signature,
        },
    }
}

fn signed_revoke(
    signer: &ed25519::PrivateKey,
    owner_account: u64,
    name: &str,
    account: u64,
) -> GatewayMsg {
    let statement = grant_statement(owner_account, name, account);
    let preimage = revoke_credential_preimage(&statement).unwrap();
    let signature = signer
        .sign(GATEWAY_CREDENTIAL_NS, &preimage)
        .as_ref()
        .to_vec();
    GatewayMsg::RevokeCredential {
        statement,
        authorization: MemberAuthorization {
            signer: pubkey(signer),
            signature,
        },
    }
}

fn query_credential(module: &Gateway, name: &str) -> Option<CredentialRecord> {
    let reply = block_on(module.query(&encode_query(&GatewayQuery::Credential {
        name: name.into(),
    })))
    .unwrap();
    match decode_reply(&reply).unwrap() {
        GatewayReply::Credential(record) => record,
        other => panic!("expected a credential reply, got {other:?}"),
    }
}

#[test]
fn credential_wire_round_trips() {
    let signer = ed25519::PrivateKey::from_seed(31);
    let record = credential("alice-claude-1", 5, node());
    let msg = signed_set_credential(&signer, record);
    let decoded = decode_msg(&encode_msg(&msg)).expect("decode");
    assert_eq!(msg, decoded);
}

#[test]
fn credential_names_are_validated() {
    let too_long = "x".repeat(65);
    for bad in ["", "UPPER", "has space", too_long.as_str()] {
        assert!(
            validate_credential_name(bad).is_err(),
            "{bad:?} must be rejected"
        );
    }
    assert!(validate_credential_name("alice-claude-1").is_ok());
}

#[test]
fn first_registration_wins_and_owner_gates_mutations() {
    let node_a = vec![1; 32];
    let node_b = vec![2; 32];
    let signer_a = ed25519::PrivateKey::from_seed(100);
    let signer_b = ed25519::PrivateKey::from_seed(200);
    let account_a = account(1, &signer_a);
    let account_b = account(2, &signer_b);
    let mut context = TestCtx::new(
        pubkey(&signer_a),
        BTreeMap::from([
            (pubkey(&signer_a), account_a.clone()),
            (pubkey(&signer_b), account_b.clone()),
        ]),
    );
    let mut module = gateway();

    // owner A registers "a".
    context.act_as(pubkey(&signer_a));
    execute(
        &mut module,
        &mut context,
        signed_set_credential(&signer_a, credential("a", account_a.number, node_a.clone())),
    )
    .unwrap();
    block_on(module.commit_block()).unwrap();

    // account B cannot squat the same name.
    context.act_as(pubkey(&signer_b));
    assert!(
        execute(
            &mut module,
            &mut context,
            signed_set_credential(&signer_b, credential("a", account_b.number, node_b.clone())),
        )
        .unwrap_err()
        .to_string()
        .contains("already registered")
    );

    // a non-owner grant is refused; an owner grant commits.
    assert!(
        execute(
            &mut module,
            &mut context,
            signed_grant(&signer_b, account_b.number, "a", account_b.number),
        )
        .is_err()
    );
    context.act_as(pubkey(&signer_a));
    execute(
        &mut module,
        &mut context,
        signed_grant(&signer_a, account_a.number, "a", account_b.number),
    )
    .unwrap();
    block_on(module.commit_block()).unwrap();
    let record = query_credential(&module, "a").expect("record");
    assert!(credential_use_allowed(&record, account_b.number));

    // owner revokes, then removes.
    execute(
        &mut module,
        &mut context,
        signed_revoke(&signer_a, account_a.number, "a", account_b.number),
    )
    .unwrap();
    block_on(module.commit_block()).unwrap();
    assert!(!credential_use_allowed(
        &query_credential(&module, "a").unwrap(),
        account_b.number
    ));

    execute(
        &mut module,
        &mut context,
        signed_remove(&signer_a, account_a.number, "a"),
    )
    .unwrap();
    block_on(module.commit_block()).unwrap();
    assert!(query_credential(&module, "a").is_none());
}

#[test]
fn same_owner_reregistration_carries_existing_grants_forward() {
    let node_a = vec![1; 32];
    let signer_a = ed25519::PrivateKey::from_seed(101);
    let signer_b = ed25519::PrivateKey::from_seed(201);
    let account_a = account(1, &signer_a);
    let account_b = account(2, &signer_b);
    let mut context = TestCtx::new(
        pubkey(&signer_a),
        BTreeMap::from([
            (pubkey(&signer_a), account_a.clone()),
            (pubkey(&signer_b), account_b.clone()),
        ]),
    );
    let mut module = gateway();

    // owner A registers "a", then grants B use of it.
    context.act_as(pubkey(&signer_a));
    execute(
        &mut module,
        &mut context,
        signed_set_credential(&signer_a, credential("a", account_a.number, node_a.clone())),
    )
    .unwrap();
    execute(
        &mut module,
        &mut context,
        signed_grant(&signer_a, account_a.number, "a", account_b.number),
    )
    .unwrap();
    block_on(module.commit_block()).unwrap();
    assert!(credential_use_allowed(
        &query_credential(&module, "a").unwrap(),
        account_b.number
    ));

    // owner A re-registers the same name (e.g. a rotated token) with a
    // fresh, grant-free statement: the committed grant must survive.
    execute(
        &mut module,
        &mut context,
        signed_set_credential(&signer_a, credential("a", account_a.number, node_a)),
    )
    .unwrap();
    block_on(module.commit_block()).unwrap();
    assert!(credential_use_allowed(
        &query_credential(&module, "a").unwrap(),
        account_b.number
    ));
}
