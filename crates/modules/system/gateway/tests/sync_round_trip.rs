//! state-sync round-trip: a joiner reconstructs a byte-identical qmdb root by
//! pulling a source store's operation range through commonware's qmdb sync,
//! then wraps a fresh `Gateway` around the injected store — the same
//! discriminating property chat, pages, governance, identity, and capability
//! prove, over the handle + route + credential layout.
//!
//! the source SEEDS the `__config` chain-id record exactly the way the
//! production genesis path does (`bin/node/src/host_state.rs`
//! `seed_store_config`), registers and RENAMES a handle (the op log carries
//! index deletes), publishes a route and replaces it at the next revision
//! (record overwrite), and registers/grants/removes credentials, so the
//! joiner must reconstruct every record family the module stores.

use std::collections::BTreeSet;

use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use gateway::{
    CredentialGrantStatement, CredentialKind, CredentialRecord, DuckDnsName, GATEWAY_CREDENTIAL_NS,
    GATEWAY_ROUTE_NS, Gateway, GatewayMsg, GatewayQuery, GatewayReply, MemberAuthorization,
    RemoveCredentialStatement, RouteAudience, RouteDefinition, RouteMethod, RouteName, RoutePolicy,
    RouteStatement, RouteTarget, SetCredentialStatement, decode_reply, encode_msg, encode_query,
    grant_credential_preimage, remove_credential_preimage, route_signing_preimage,
    set_credential_preimage,
};
use identity::{
    AccountView, IdentityQuery, IdentityReply, KeyScheme, KeyView,
    decode_query as identity_decode_query, encode_reply as identity_encode_reply,
};
use sdk::{Env, Error, MerkleStore as _, Module, Msg, Origin, StateRoot};
use sdk_testkit::TestCtx;
use statesync::qmdb::QmdbStore;

const CHAIN: &str = "sync-chain";
/// the founder's account number.
const ACCOUNT: u64 = 1;

type Ed = PrivateKey;

fn ed(seed: u64) -> Ed {
    Ed::from_seed(seed)
}
fn ed_pub(k: &Ed) -> Vec<u8> {
    k.public_key().as_ref().to_vec()
}

/// the one identity read gateway makes (`OfKey`), answered with a fixed
/// account whose sole member is `founder` — the TestCtx double for the
/// sibling the parity proof exercises for real.
fn account_view(founder: &Ed) -> AccountView {
    AccountView {
        control: identity::Control::Keys,
        number: ACCOUNT,
        name: "founder".into(),
        keys: vec![KeyView {
            scheme: KeyScheme::Ed25519,
            pubkey: ed_pub(founder),
            label: None,
            added_at: 0,
        }],
        avatar: None,
        bio: None,
        updated_at: 0,
    }
}

/// the founder's key is the frame origin of every op.
fn ctx(height: u64, founder: &Ed) -> TestCtx {
    let view = account_view(founder);
    TestCtx::with_env(Env {
        height,
        consensus_time: height,
        origin: Origin::External(ed_pub(founder)),
        me: "gateway".into(),
        cause: sdk::Cause::Direct,
    })
    .on_query("identity", move |req| {
        match identity_decode_query(req).map_err(|e| Error::module("codec", e))? {
            IdentityQuery::OfKey { .. } => Ok(identity_encode_reply(&IdentityReply::Account(
                Some(view.clone()),
            ))),
            _ => Err(Error::QueryUnsupported),
        }
    })
}

fn gw(m: &GatewayMsg) -> Msg {
    Msg {
        target: "gateway".into(),
        payload: encode_msg(m),
    }
}

fn content_route(seed: u8) -> RouteDefinition {
    RouteDefinition {
        target: RouteTarget::DuckFs {
            manifest_sha256: format!("{seed:02x}").repeat(32),
        },
        policy: RoutePolicy {
            audience: RouteAudience::Network,
            methods: vec![RouteMethod::Get, RouteMethod::Head],
            max_request_bytes: Some(0),
            max_response_bytes: 1024 * 1024,
            allow_authorization: false,
            allow_upgrade: false,
        },
    }
}

fn set_route(founder: &Ed, node: &[u8], revision: u64, route: Option<RouteDefinition>) -> Msg {
    let statement = RouteStatement {
        chain_id: CHAIN.into(),
        account_id: ACCOUNT,
        name: RouteName::apex(),
        publisher_node: node.to_vec(),
        revision,
        route,
    };
    let preimage = route_signing_preimage(&statement).expect("statement validates");
    let authorization = MemberAuthorization {
        signer: ed_pub(founder),
        signature: founder.sign(GATEWAY_ROUTE_NS, &preimage).as_ref().to_vec(),
    };
    gw(&GatewayMsg::SetRoute {
        statement,
        authorization,
    })
}

fn set_credential(founder: &Ed, node: &[u8], name: &str) -> Msg {
    let statement = SetCredentialStatement {
        chain_id: CHAIN.into(),
        record: CredentialRecord {
            name: name.into(),
            owner_account: ACCOUNT,
            publisher_node: node.to_vec(),
            kind: CredentialKind::Claude,
            seal_pk: [7u8; 32],
            grants: BTreeSet::new(),
        },
    };
    let preimage = set_credential_preimage(&statement).expect("statement validates");
    let authorization = MemberAuthorization {
        signer: ed_pub(founder),
        signature: founder
            .sign(GATEWAY_CREDENTIAL_NS, &preimage)
            .as_ref()
            .to_vec(),
    };
    gw(&GatewayMsg::SetCredential {
        statement,
        authorization,
    })
}

fn grant_credential(founder: &Ed, name: &str, account: u64) -> Msg {
    let statement = CredentialGrantStatement {
        chain_id: CHAIN.into(),
        owner_account: ACCOUNT,
        name: name.into(),
        account,
    };
    let preimage = grant_credential_preimage(&statement).expect("statement validates");
    let authorization = MemberAuthorization {
        signer: ed_pub(founder),
        signature: founder
            .sign(GATEWAY_CREDENTIAL_NS, &preimage)
            .as_ref()
            .to_vec(),
    };
    gw(&GatewayMsg::GrantCredential {
        statement,
        authorization,
    })
}

fn remove_credential(founder: &Ed, name: &str) -> Msg {
    let statement = RemoveCredentialStatement {
        chain_id: CHAIN.into(),
        owner_account: ACCOUNT,
        name: name.into(),
    };
    let preimage = remove_credential_preimage(&statement).expect("statement validates");
    let authorization = MemberAuthorization {
        signer: ed_pub(founder),
        signature: founder
            .sign(GATEWAY_CREDENTIAL_NS, &preimage)
            .as_ref()
            .to_vec(),
    };
    gw(&GatewayMsg::RemoveCredential {
        statement,
        authorization,
    })
}

fn set_handle(handle: Option<&str>) -> Msg {
    gw(&GatewayMsg::SetHandle {
        handle: handle.map(str::to_string),
    })
}

// drive one op through the REAL module path: execute + commit_block (one op
// per block-height), so the committed op log is what a validator produces.
async fn apply_commit(m: &mut Gateway, height: u64, founder: &Ed, op: Msg) {
    let mut c = ctx(height, founder);
    m.execute(&mut c, &op).await.unwrap();
    m.commit_block().await.unwrap();
}

/// the read matrix compared source-vs-joiner: both handle resolutions (live
/// and the renamed-away name), the registration listing, the route point
/// read, the per-account listing, and both credential views.
const QUERIES: [&str; 7] = [
    "resolve-live",
    "resolve-renamed",
    "registrations",
    "route",
    "routes",
    "credential",
    "credentials",
];

async fn replies(m: &Gateway) -> Vec<GatewayReply> {
    let queries = [
        encode_query(&GatewayQuery::Resolve {
            name: DuckDnsName {
                handle: "quack".into(),
            },
        }),
        encode_query(&GatewayQuery::Resolve {
            name: DuckDnsName {
                handle: "orthory".into(),
            },
        }),
        encode_query(&GatewayQuery::Registrations { from: 0, limit: 16 }),
        encode_query(&GatewayQuery::Get {
            account_id: ACCOUNT,
            name: RouteName::apex(),
        }),
        encode_query(&GatewayQuery::List {
            account_id: ACCOUNT,
        }),
        encode_query(&GatewayQuery::Credential {
            name: "anthropic".into(),
        }),
        encode_query(&GatewayQuery::Credentials {}),
    ];
    let mut out = Vec::new();
    for q in &queries {
        out.push(decode_reply(&m.query(q).await.unwrap()).unwrap());
    }
    out
}

/// the production wiring shape (the round trip proves the record layout; the
/// member gate is pinned by the parity proof).
fn gateway_over(store: Box<dyn sdk::MerkleStore>) -> Gateway {
    Gateway::new("gateway", store, "identity", CHAIN)
}

#[test]
fn synced_store_reconstructs_source_root_handles_routes_and_credentials() {
    deterministic::Runner::default().start(|context| async move {
        let founder = ed(1);
        let node = ed_pub(&ed(2));
        let grantee: u64 = 3;

        // SOURCE: seed the genesis-config record the way the production
        // genesis path does, THEN wrap the module — the config is committed
        // store state under the shared `sdk::store_key` convention, part of
        // the root from block zero.
        let mut src_store = QmdbStore::init(context.child("src"), "src").await;
        let config = sdk::genesis_config::encode_config(&[("chain_id", CHAIN.as_bytes())]);
        src_store
            .commit_batch(vec![(
                sdk::store_key(sdk::genesis_config::CONFIG_KEY),
                Some(config.clone()),
            )])
            .await
            .expect("seed genesis config");
        let config_root = src_store.root();
        assert_ne!(config_root, StateRoot::ZERO, "config alone moves the root");
        let mut src = gateway_over(Box::new(src_store));

        // handle: register, then RENAME (the op log carries the old name's
        // index delete, not just inserts).
        apply_commit(&mut src, 1, &founder, set_handle(Some("orthory"))).await;
        apply_commit(&mut src, 2, &founder, set_handle(Some("quack"))).await;
        // route: publish at revision 1, replace at revision 2 (overwrite).
        apply_commit(
            &mut src,
            3,
            &founder,
            set_route(&founder, &node, 1, Some(content_route(0x11))),
        )
        .await;
        apply_commit(
            &mut src,
            4,
            &founder,
            set_route(&founder, &node, 2, Some(content_route(0x22))),
        )
        .await;
        // credentials: register + grant (overwrite), and an insert-then-remove
        // pair so the credential roster carries a delete too.
        apply_commit(
            &mut src,
            5,
            &founder,
            set_credential(&founder, &node, "anthropic"),
        )
        .await;
        apply_commit(
            &mut src,
            6,
            &founder,
            grant_credential(&founder, "anthropic", grantee),
        )
        .await;
        apply_commit(
            &mut src,
            7,
            &founder,
            set_credential(&founder, &node, "extra"),
        )
        .await;
        apply_commit(&mut src, 8, &founder, remove_credential(&founder, "extra")).await;

        let src_root: StateRoot = src.root();
        assert_ne!(src_root, config_root, "the ops moved the root");
        let src_replies = replies(&src).await;

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
        // exact shape a joining host uses.
        let store = QmdbStore::sync_from(context.child("dst"), "dst", target, resolver)
            .await
            .expect("sync_from");

        // the genesis-config record ARRIVED with the op range: this is what a
        // joiner's wasm guest reads its chain id from.
        assert_eq!(
            store
                .get(&sdk::store_key(sdk::genesis_config::CONFIG_KEY))
                .await
                .expect("config read"),
            Some(config),
            "the __config record rides the sync"
        );
        let synced = gateway_over(Box::new(store));

        // THE PROPERTY: identical qmdb root — the root-hash linkage a joiner
        // needs at the boundary height.
        assert_eq!(
            synced.root(),
            src_root,
            "synced store root must equal the source root"
        );

        // handles (with the rename's delete), the route at revision 2, and
        // the surviving credential (with its grant) synced together: the
        // joiner answers every read exactly like the source.
        let synced_replies = replies(&synced).await;
        for (name, (a, b)) in QUERIES.iter().zip(src_replies.iter().zip(&synced_replies)) {
            assert_eq!(a, b, "the {name} reply diverged");
        }
        let GatewayReply::Resolved(Some(resolved)) = &synced_replies[0] else {
            panic!("the live handle must resolve on the joiner");
        };
        assert_eq!(resolved.account_id, ACCOUNT);
        let GatewayReply::Resolved(None) = &synced_replies[1] else {
            panic!("the renamed-away handle must stay free on the joiner");
        };
        let GatewayReply::Route(route) = &synced_replies[3] else {
            panic!("the route must be present on the joiner");
        };
        assert_eq!(
            route.as_ref().as_ref().map(|r| r.statement.revision),
            Some(2),
            "the revision-2 replacement is what synced"
        );
        let GatewayReply::Credentials(credentials) = &synced_replies[6] else {
            panic!("the credential listing must be present on the joiner");
        };
        assert_eq!(credentials.len(), 1, "the removed credential stayed gone");
        assert!(credentials[0].grants.contains(&grantee), "the grant synced");
    });
}
