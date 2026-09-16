//! Two-node granted-credential lending over the gateway overlay, and the
//! owner-gateway grant gate the flow forces.
//!
//! The OWNER node (0) runs the airlock LENDER DAEMON (`ducktape service run
//! airlock`) beside it, serving its disk-backed credential store (the `user cred
//! add` layout) on a loopback port the node reverse-proxies overlay ingress to.
//! The owner's USER founds an account, registers the credential record on-chain
//! and grants it to the COMPUTE user's account (the user whose node is 1). Every
//! one of those is a user-signed frame: the gateway attributes an op to its
//! frame origin through identity's `OfKey`, and a node key is on no account.
//!
//! A node is never an account, so the hop itself carries no grant: the ONE way
//! a session opens at the lender is DELEGATION — the caller presents a pointer
//! to committed work whose user-signed origin is on the grant (the owner
//! itself, or a grantee), PINNED to the calling node, NAMING this credential,
//! and still running. Each of those gets its own negative here — a session
//! naming no work at all, the wrong executor, another credential, a saga the
//! lender cannot resolve, and (the sharpest) the very pointer that just worked,
//! replayed once its saga is terminal. Then the grant: the compute user's own
//! work, pinned to its node, is refused before the grant and draws after it —
//! the grant is the only thing that changes. This lane needs no sandbox, so it
//! can drive shapes no product path constructs. The live end-to-end delegated
//! RUN is `sched_pinned_run::a_delegated_run_draws_on_the_submitters_grant`.
//!
//! Like `airlock_gateway_e2e`, this proves the node-to-node overlay hop with a
//! mock upstream (no Anthropic quota, no silicon). The self-host trust anchor is
//! the on-chain seal_pk, pinned by the client — there is no TEE quote here.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Cluster, create_account, submit_frame};
use commonware_cryptography::{Signer as _, ed25519};
use gateway::{
    CredentialGrantStatement, CredentialKind, CredentialRecord, DuckDnsName, GATEWAY_CREDENTIAL_NS,
    GatewayMsg, GatewayQuery, GatewayReply, MemberAuthorization, RouteAudience, RouteDefinition,
    RouteMethod, RouteName, RoutePolicy, RouteStatement, RouteTarget, SetCredentialStatement,
    credential_use_allowed,
};

use airlock::client::Gateway as AirlockClient;
use airlock::seal::SealKeypair;
use airlock::wire::WorkRef;
use saga::{SagaMsg, SagaQuery, SagaReply, SagaView};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tokio::runtime::Runtime;

const READY: Duration = Duration::from_secs(180);
const FINALIZE: Duration = Duration::from_secs(60);

// ----- duckdns + route helpers (mirror airlock_gateway_e2e) -----------------

fn resolve_handle(cluster: &Cluster, reader: usize, handle: &str) -> Option<u64> {
    let bytes = cluster.query(
        reader,
        "gateway",
        &gateway::encode_query(&GatewayQuery::Resolve {
            name: DuckDnsName {
                handle: handle.into(),
            },
        }),
    )?;
    match gateway::decode_reply(&bytes).ok()? {
        GatewayReply::Resolved(Some(account)) => Some(account.account_id),
        _ => None,
    }
}

/// The airlock LoopbackHttp route: `allow_authorization = true` (the session-token
/// bearer must reach the enclave) and a real 4 MiB response cap. Same shape as
/// `airlock_gateway_e2e`'s route.
fn signed_airlock_route(
    member: &ed25519::PrivateKey,
    chain: &str,
    account: u64,
    publisher: &[u8],
    revision: u64,
) -> GatewayMsg {
    let statement = RouteStatement {
        chain_id: chain.into(),
        account_id: account,
        name: RouteName::named("airlock"),
        publisher_node: publisher.to_vec(),
        revision,
        route: Some(RouteDefinition {
            target: RouteTarget::LoopbackHttp,
            policy: RoutePolicy {
                audience: RouteAudience::Network,
                methods: vec![RouteMethod::Get, RouteMethod::Head, RouteMethod::Post],
                max_request_bytes: Some(1024 * 1024),
                max_response_bytes: 4 * 1024 * 1024,
                allow_authorization: true,
                allow_upgrade: false,
            },
        }),
    };
    let signature = member
        .sign(
            gateway::GATEWAY_ROUTE_NS,
            &gateway::route_signing_preimage(&statement).unwrap(),
        )
        .as_ref()
        .to_vec();
    GatewayMsg::SetRoute {
        statement,
        authorization: MemberAuthorization {
            signer: member.public_key().as_ref().to_vec(),
            signature,
        },
    }
}

fn airlock_route_revision(cluster: &Cluster, reader: usize, account: u64) -> Option<u64> {
    let bytes = cluster.query(
        reader,
        "gateway",
        &gateway::encode_query(&GatewayQuery::Get {
            account_id: account,
            name: RouteName::named("airlock"),
        }),
    )?;
    match gateway::decode_reply(&bytes).ok()? {
        GatewayReply::Route(record) => record
            .as_ref()
            .as_ref()
            .map(|record| record.statement.revision),
        _ => None,
    }
}

// ----- credential record helpers (Task 1 wire) ------------------------------

/// The owner-signed registration for a claude credential, publisher = the owner
/// node, `grants` empty (grants are added by `signed_grant`). Signed under
/// [`GATEWAY_CREDENTIAL_NS`] exactly as the module verifies it.
fn signed_set_credential(
    owner: &ed25519::PrivateKey,
    chain: &str,
    owner_account: u64,
    publisher_node: &[u8],
    name: &str,
    seal_pk: [u8; 32],
) -> GatewayMsg {
    let statement = SetCredentialStatement {
        chain_id: chain.into(),
        record: CredentialRecord {
            name: name.into(),
            owner_account,
            publisher_node: publisher_node.to_vec(),
            kind: CredentialKind::Claude,
            seal_pk,
            grants: std::collections::BTreeSet::new(),
        },
    };
    let signature = owner
        .sign(
            GATEWAY_CREDENTIAL_NS,
            &gateway::set_credential_preimage(&statement).unwrap(),
        )
        .as_ref()
        .to_vec();
    GatewayMsg::SetCredential {
        statement,
        authorization: MemberAuthorization {
            signer: owner.public_key().as_ref().to_vec(),
            signature,
        },
    }
}

/// The owner-signed grant of `name` to account `grantee`.
fn signed_grant(
    owner: &ed25519::PrivateKey,
    chain: &str,
    owner_account: u64,
    name: &str,
    grantee: u64,
) -> GatewayMsg {
    let statement = CredentialGrantStatement {
        chain_id: chain.into(),
        owner_account,
        name: name.into(),
        account: grantee,
    };
    let signature = owner
        .sign(
            GATEWAY_CREDENTIAL_NS,
            &gateway::grant_credential_preimage(&statement).unwrap(),
        )
        .as_ref()
        .to_vec();
    GatewayMsg::GrantCredential {
        statement,
        authorization: MemberAuthorization {
            signer: owner.public_key().as_ref().to_vec(),
            signature,
        },
    }
}

fn query_credential(cluster: &Cluster, reader: usize, name: &str) -> Option<CredentialRecord> {
    let bytes = cluster.query(
        reader,
        "gateway",
        &gateway::encode_query(&GatewayQuery::Credential {
            name: name.to_string(),
        }),
    )?;
    match gateway::decode_reply(&bytes).ok()? {
        GatewayReply::Credential(record) => record,
        _ => None,
    }
}

/// Trigger a saga through node `idx` as a frame `submitter` signed — its USER
/// key stamps the committed origin, which is what the lender resolves to an
/// account — PINNED to `target` and naming `credential`, exactly the shape
/// `agent sched --cred --host-node` composes (through the same two producers,
/// so the gate and the composer cannot drift apart here).
///
/// Nothing executes it: this cluster runs no compute daemon. The saga exists only
/// to be POINTED AT, so its lease window is wide enough that no crank can
/// re-lease it out from under the assertions.
fn trigger_pinned_saga(
    cluster: &Cluster,
    idx: usize,
    submitter: &ed25519::PrivateKey,
    saga_id: &str,
    target: &[u8],
    credential: &str,
) {
    let spec = dispatch::encode_work_spec(&dispatch::WorkSpec {
        kind: dispatch::WORK_SPEC_KIND.into(),
        dispatch_id: saga_id.rsplit('\u{1f}').next().unwrap().into(),
        capability: "never-claimed".into(),
        payload: compute_service::envelope::compose_headless(saga_id, "PING", Some(credential))
            .into_bytes(),
        demands: Default::default(),
        admission: dispatch::AdmissionPolicy::Queue,
    });
    submit_frame(
        cluster,
        idx,
        submitter,
        "saga",
        &saga::encode_msg(&SagaMsg::Trigger {
            saga_id: saga_id.into(),
            spec,
            reply_to: None,
            reply_payload: Vec::new(),
            deadline: None,
            max_attempts: 1,
            lease_views: Some(1_000_000),
            capability: Some("never-claimed".into()),
            demands: Default::default(),
            pinned_assignee: Some(target.to_vec()),
        }),
    );
}

/// saga's id space is namespaced per trigger origin — the frame's USER key —
/// so an id `user` triggers lives under that key's actor namespace, and no
/// other signer can create it.
fn user_sid(user: &ed25519::PrivateKey, id: &str) -> String {
    let key = user.public_key().as_ref().to_vec();
    saga::namespaced_id(&sdk::Origin::External(key), id)
}

/// Cancel a saga as the user that triggered it — the saga module admits a
/// cancel only from the recorded origin, so this is the owner retiring its own
/// work. The cheapest way to reach a TERMINAL saga on a cluster that executes
/// nothing.
fn cancel_saga(cluster: &Cluster, idx: usize, submitter: &ed25519::PrivateKey, saga_id: &str) {
    submit_frame(
        cluster,
        idx,
        submitter,
        "saga",
        &saga::encode_msg(&SagaMsg::Cancel {
            saga_id: saga_id.into(),
        }),
    );
}

/// Open a session against the owner's real lender through node 1's browser
/// gateway — the only path production uses, and the one that makes the lender
/// stamp node 1 as the vouched-for caller.
fn open_session_as_node_1(
    rt: &Runtime,
    via: &str,
    seal_pk: &[u8; 32],
    credential: &str,
    work: WorkRef,
) -> Result<String, anyhow::Error> {
    rt.block_on(async {
        AirlockClient::remote("airlock.owner.duck".into(), via.to_string())
            .open_session(seal_pk, credential, &work)
            .await
    })
}

fn saga_view(cluster: &Cluster, reader: usize, saga_id: &str) -> Option<SagaView> {
    let bytes = cluster.query(
        reader,
        "saga",
        &saga::encode_query(&SagaQuery::Get {
            saga_id: saga_id.into(),
        }),
    )?;
    match saga::decode_reply(&bytes).ok()? {
        SagaReply::Saga(view) => view,
        _ => None,
    }
}

/// Wait until the LENDER's node has committed the saga with the expected
/// assignee. Both halves matter: the lender answers from its OWN state, so a
/// pointer it has not committed yet is undecidable rather than refused, and this
/// is the event the delegated assertions below wait on.
fn wait_leased(cluster: &Cluster, saga_id: &str, assignee: &[u8]) {
    cluster.await_committed(
        0,
        "the lender to commit the pinned saga's lease",
        FINALIZE,
        || saga_view(cluster, 0, saga_id).filter(|view| view.assignee.as_deref() == Some(assignee)),
    );
}

/// Seed the owner's disk-backed store with a claude credential dir (the layout the
/// lender's `load_seeds` reads: a `kind` marker + a `.credentials.json` refresh
/// token). Done BEFORE the daemon spawns so it is served from its first session.
fn seed_claude_store(storage: &std::path::Path, name: &str, refresh: &str) {
    let dir = storage.join("airlock-creds").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("kind"), "claude\n").unwrap();
    std::fs::write(
        dir.join(".credentials.json"),
        format!(r#"{{"claudeAiOauth":{{"refreshToken":"{refresh}"}}}}"#),
    )
    .unwrap();
}

/// The seal PUBLIC key the owner's lender daemon minted when it opened the store
/// — read from the store's `seal.key` (32-byte secret, 0600). This is the anchor
/// the owner puts on-chain and the compute node pins.
fn seal_pk_from_store(storage: &std::path::Path) -> [u8; 32] {
    let bytes = std::fs::read(storage.join("airlock-creds").join("seal.key")).expect("seal.key");
    let secret: [u8; 32] = bytes.as_slice().try_into().expect("32-byte seal secret");
    SealKeypair::from_secret_bytes(secret).public_bytes()
}

// ----- mock Anthropic upstream (mirror airlock_gateway_e2e) -----------------

/// `/oauth/token` mints `acc-N`; `/v1/messages` accepts ONLY `Bearer acc-N` (so a
/// 200 proves the gateway refreshed the lent credential and swapped the session
/// token for the real one) and returns the `AIRLOCK-OK` marker.
#[derive(Default)]
struct MockUpstream {
    n: Mutex<u64>,
}

async fn mock_oauth(State(st): State<Arc<MockUpstream>>) -> Json<serde_json::Value> {
    let mut n = st.n.lock().unwrap();
    *n += 1;
    Json(
        json!({ "access_token": format!("acc-{n}"), "refresh_token": format!("ref-{n}"), "expires_in": 3600 }),
    )
}

async fn mock_messages(
    State(st): State<Arc<MockUpstream>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let want = format!("Bearer acc-{}", *st.n.lock().unwrap());
    let got = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got != want {
        return (axum::http::StatusCode::UNAUTHORIZED, "bad upstream bearer").into_response();
    }
    (
        [("content-type", "text/event-stream")],
        "data: AIRLOCK-OK\n\n",
    )
        .into_response()
}

async fn bind_and_serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[test]
fn granted_credential_resolves_and_round_trips_across_nodes() {
    let rt = Runtime::new().unwrap();

    // The owner's mock Anthropic upstream lives in THIS process; the owner node
    // subprocess reaches it over host loopback, exactly like a real deployment
    // reaches api.anthropic.com.
    let (upstream, oauth_url) = rt.block_on(async {
        let base = bind_and_serve(
            Router::new()
                .route("/oauth/token", post(mock_oauth))
                .route("/v1/messages", post(mock_messages))
                .with_state(Arc::new(MockUpstream::default())),
        )
        .await;
        let oauth = format!("{base}/oauth/token");
        (base, oauth)
    });

    // Two real WireGuard nodes: owner (0, publisher) and compute (1, grantee).
    let mut cluster = Cluster::new(&[0, 1], &[0, 1]);
    cluster.wireguard = true;

    // The owner co-hosts the store: seed the credential dir BEFORE the lender
    // daemon starts so it is served from the first session, and point the
    // daemon's upstream + oauth at the mock.
    let owner_storage = cluster.workspace(0);
    seed_claude_store(&owner_storage, "owner-claude-1", "rt-e2e");
    let owner_key_file = owner_storage.join("owner.key");
    let (_, owner) = keystore::userkey::mint_user_key(&owner_key_file, "lender-fixture-password")
        .expect("mint the lender operator's encrypted wallet");
    cluster.env[0] = vec![
        (
            "DUCKTAPE_USER_KEY".into(),
            owner_key_file.display().to_string(),
        ),
        ("DUCKTAPE_AIRLOCK_ANTHROPIC_BASE".into(), upstream.clone()),
        ("DUCKTAPE_AIRLOCK_OAUTH_TOKEN_URL".into(), oauth_url.clone()),
    ];

    for index in 0..2 {
        cluster.spawn(index);
    }
    for index in 0..2 {
        cluster.wait_marker(index, "rpc listening on", READY);
        cluster.wait_marker(index, "converged root_hash=", READY);
        cluster.wait_marker(index, "peer handshake COMPLETE", READY);
        cluster.wait_marker(index, "gateway plane: overlay stream bound", READY);
    }
    // Admit the lender's actual operator wallet before the service resolves
    // its route account. Node identities remain separate, accountless keys.
    let compute = ed25519::PrivateKey::from_seed(43);
    let owner_node = Cluster::identity(0);
    let compute_node = Cluster::identity(1);
    let owner_account = create_account(&cluster, 0, &owner, "owner");
    let compute_account = create_account(&cluster, 1, &compute, "compute");

    // Only NOW start the lender: the daemon's first hello must land, so its
    // node's http surface has to be listening before it starts. It opens the
    // store (minting seal.key), binds loopback and registers its port as the
    // `airlock` gateway route the node reverse-proxies to.
    cluster.spawn_service(0, "airlock");
    cluster.wait_service_marker(0, "airlock", "airlock daemon serving", READY);

    // Owner maps `owner.duck`, so `airlock.owner.duck` resolves to her account.
    submit_frame(
        &cluster,
        0,
        &owner,
        "gateway",
        &gateway::encode_msg(&GatewayMsg::SetHandle {
            handle: Some("owner".into()),
        }),
    );
    for reader in 0..2 {
        cluster.await_committed(reader, "owner.duck resolution", FINALIZE, || {
            resolve_handle(&cluster, reader, "owner").filter(|id| *id == owner_account)
        });
    }

    // Publish the signed airlock route (the one manual operator act; the daemon
    // already registered its loopback port).
    submit_frame(
        &cluster,
        0,
        &owner,
        "gateway",
        &gateway::encode_msg(&signed_airlock_route(
            &owner,
            &cluster.namespace,
            owner_account,
            &owner_node,
            1,
        )),
    );
    cluster.await_committed(1, "airlock route revision 1", FINALIZE, || {
        (airlock_route_revision(&cluster, 1, owner_account) == Some(1)).then_some(())
    });

    // Register the credential record on-chain with the store's seal_pk, then grant
    // it to the compute account. The seal.key exists once the daemon opened the store.
    let seal_pk = seal_pk_from_store(&owner_storage);
    submit_frame(
        &cluster,
        0,
        &owner,
        "gateway",
        &gateway::encode_msg(&signed_set_credential(
            &owner,
            &cluster.namespace,
            owner_account,
            &owner_node,
            "owner-claude-1",
            seal_pk,
        )),
    );
    cluster.await_committed(1, "credential record committed", FINALIZE, || {
        query_credential(&cluster, 1, "owner-claude-1")
    });

    // A session naming NO work has nobody's grant to draw on: the hop vouches
    // for a node, a node is never an account, and there is no pointer to
    // resolve a submitter from. Refused, and named as the grant that is missing.
    // Cheap on purpose — this lane needs no sandbox, so the property stays
    // provable on a host that cannot run one.
    let record_ungranted = query_credential(&cluster, 1, "owner-claude-1").expect("record");
    let (status, browser) = cluster.http(1, "GET", "/v1/gateway/browser", None);
    assert_eq!(status, 200, "browser base failed: {browser}");
    let via_pre = browser["base"].as_str().unwrap().to_string();
    let refused_direct = rt.block_on(async {
        AirlockClient::remote("airlock.owner.duck".into(), via_pre.clone())
            .open_session(
                &record_ungranted.seal_pk,
                "owner-claude-1",
                &WorkRef::Direct,
            )
            .await
    });
    let refused_direct = refused_direct.expect_err("a session naming no work must not open");
    assert!(
        format!("{refused_direct}").contains("credential_not_granted"),
        "and it is a GRANT that is missing, named as such: {refused_direct}"
    );

    // ---- DELEGATION -------------------------------------------------------
    //
    // The SAME caller node, and nothing below grants its user anything. What
    // changes is that it presents a POINTER: a saga the OWNER submitted — so
    // its committed origin is the owner's USER key, proven by the frame
    // signature, which `OfKey` resolves to the owner's account — pinned to the
    // compute node and naming this credential. The lender resolves ALL of that
    // out of its own committed state; this side asserts none of it and cannot.
    let seal = record_ungranted.seal_pk;
    let delegated = &user_sid(&owner, "sched\u{1f}cred-lending-delegated");
    trigger_pinned_saga(
        &cluster,
        0,
        &owner,
        delegated,
        &compute_node,
        "owner-claude-1",
    );
    wait_leased(&cluster, delegated, &compute_node);
    open_session_as_node_1(
        &rt,
        &via_pre,
        &seal,
        "owner-claude-1",
        WorkRef::Saga {
            saga_id: delegated.into(),
        },
    )
    .expect("an ungranted executor draws on the SUBMITTER's grant for work it holds");

    // NEGATIVE — the EXECUTOR condition, the one a "simplification" would drop.
    // This saga's ORIGIN is the very same owner, so a gate checking only the
    // origin admits it: every saga the owner ever submitted would become a key to
    // the owner's subscription, for work the owner never assigned to this node.
    let owners_own = &user_sid(&owner, "sched\u{1f}cred-lending-owners-own");
    trigger_pinned_saga(
        &cluster,
        0,
        &owner,
        owners_own,
        &owner_node,
        "owner-claude-1",
    );
    wait_leased(&cluster, owners_own, &owner_node);
    let not_the_executor = open_session_as_node_1(
        &rt,
        &via_pre,
        &seal,
        "owner-claude-1",
        WorkRef::Saga {
            saga_id: owners_own.into(),
        },
    )
    .expect_err("a pointer to work this caller was not pinned to is no grant");
    assert!(
        format!("{not_the_executor}").contains("credential_not_granted"),
        "pointing at somebody else's saga is a refusal, named as one: {not_the_executor}"
    );

    // NEGATIVE — the pointer buys ONE credential, the one the committed work
    // names. Without this condition a single lease on the owner's saga opens a
    // session for any credential any lender serves that the owner is granted on,
    // including a third party's who never saw this saga. Same submitter, same
    // executor, same pin — only the credential in the spec differs.
    let names_another = &user_sid(&owner, "sched\u{1f}cred-lending-names-another");
    trigger_pinned_saga(
        &cluster,
        0,
        &owner,
        names_another,
        &compute_node,
        "a-totally-different-credential",
    );
    wait_leased(&cluster, names_another, &compute_node);
    let wrong_credential = open_session_as_node_1(
        &rt,
        &via_pre,
        &seal,
        "owner-claude-1",
        WorkRef::Saga {
            saga_id: names_another.into(),
        },
    )
    .expect_err("work naming another credential entitles this session to nothing");
    assert!(
        format!("{wrong_credential}").contains("credential_not_granted"),
        "a pointer is not a bearer token for the whole grant: {wrong_credential}"
    );

    // NEGATIVE — and the sharpest, because it is the SAME pointer that worked
    // sixty lines up. The saga module clears no assignee and no pin on any
    // terminal path, so without a liveness condition one finished run is a
    // permanent, unmetered draw the owner has nothing to revoke: the executor
    // holds no grant, so `user cred revoke` has no subject.
    cancel_saga(&cluster, 0, &owner, delegated);
    cluster.await_committed(
        0,
        "the delegated saga to reach a terminal status",
        FINALIZE,
        || saga_view(&cluster, 0, delegated).filter(|view| view.status.is_terminal()),
    );
    let finished = open_session_as_node_1(
        &rt,
        &via_pre,
        &seal,
        "owner-claude-1",
        WorkRef::Saga {
            saga_id: delegated.into(),
        },
    )
    .expect_err("a finished run is not a standing licence");
    assert!(
        format!("{finished}").contains("credential_not_granted"),
        "the pointer that opened a session while the work ran must stop: {finished}"
    );

    // NEGATIVE — a pointer the lender cannot RESOLVE is undetermined, not
    // refused. A follower behind head sees exactly this shape, and answering 403
    // would send the borrower's operator to add a grant they may already hold —
    // the misdiagnosis the three-state taxonomy exists to prevent.
    let unresolvable = open_session_as_node_1(
        &rt,
        &via_pre,
        &seal,
        "owner-claude-1",
        WorkRef::Saga {
            saga_id: user_sid(&owner, "sched\u{1f}never-committed"),
        },
    )
    .expect_err("a saga the lender has not committed decides nothing");
    assert!(
        format!("{unresolvable}").contains("503")
            && format!("{unresolvable}").contains("grant_authority_unavailable"),
        "an unresolvable pointer must 503, never 403: {unresolvable}"
    );

    // ---- THE GRANT, and the only thing that changes ------------------------
    //
    // The compute USER's own work: a saga it submitted through its node (the
    // committed origin is its user key → account 2), pinned to that node and
    // naming this credential. The pointer is identical before and after the
    // grant; only the record's grant set differs.
    let own = &user_sid(&compute, "sched\u{1f}cred-lending-own");
    trigger_pinned_saga(&cluster, 1, &compute, own, &compute_node, "owner-claude-1");
    wait_leased(&cluster, own, &compute_node);
    let ungranted = open_session_as_node_1(
        &rt,
        &via_pre,
        &seal,
        "owner-claude-1",
        WorkRef::Saga {
            saga_id: own.into(),
        },
    )
    .expect_err("a submitter on no grant list draws nothing, even for its own work");
    assert!(
        format!("{ungranted}").contains("credential_not_granted"),
        "and it is the GRANT that is missing, named as such: {ungranted}"
    );

    submit_frame(
        &cluster,
        0,
        &owner,
        "gateway",
        &gateway::encode_msg(&signed_grant(
            &owner,
            &cluster.namespace,
            owner_account,
            "owner-claude-1",
            compute_account,
        )),
    );
    cluster.await_committed(1, "grant committed", FINALIZE, || {
        query_credential(&cluster, 1, "owner-claude-1")
            .filter(|record| credential_use_allowed(record, compute_account))
            .map(|_| ())
    });

    // Compute resolves the name from committed state and pins its on-chain seal_pk.
    let record = query_credential(&cluster, 1, "owner-claude-1").expect("record");
    assert_eq!(
        record.seal_pk, seal_pk,
        "the pinned seal_pk is the store's own key"
    );

    // Compute's browser-gateway origin base — the `via` a compute-side host posts
    // to (no Origin header ⇒ it passes the only guard).
    let (status, browser) = cluster.http(1, "GET", "/v1/gateway/browser", None);
    assert_eq!(status, 200, "browser base failed: {browser}");
    let via = browser["base"].as_str().unwrap().to_string();

    // THE PROOF: from compute, over the overlay, drawing on the GRANTED account
    // for its own work. Every hop is remote (compute -> overlay -> owner ->
    // loopback upstream).
    let reply = rt.block_on(async {
        let gw = AirlockClient::remote("airlock.owner.duck".into(), via.clone());
        // No account is named, and none CAN be: the client has no such call. The
        // subject is the account whose user-signed frame submitted the work the
        // session points at — the compute user's, the one just granted.
        let token = gw
            .open_session(
                &record.seal_pk,
                "owner-claude-1",
                &WorkRef::Saga {
                    saga_id: own.into(),
                },
            )
            .await
            .expect("granted session opens over the overlay");
        let resp = gw
            .route(gw.http().post(gw.url("/v1/messages")))
            .bearer_auth(&token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("content-type", "application/json")
            .body(r#"{"model":"claude-sonnet-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .expect("proxied messages over overlay");
        (resp.status(), resp.text().await.unwrap())
    });
    assert_eq!(
        reply.0,
        reqwest::StatusCode::OK,
        "granted overlay call: {reply:?}"
    );
    assert!(
        reply.1.contains("AIRLOCK-OK"),
        "the lent credential's reply must return over the overlay: {reply:?}"
    );

    // NEGATIVE, and note what it is NOT. There is no longer a test here for
    // "claim somebody else's account", because there is no longer a way to claim
    // one: `SessionRequest` carries no account and `Gateway` exposes no call that
    // takes one, so the credential-theft shape is refused by the type system
    // rather than at runtime. The account the lender authorizes is the one whose
    // user-signed frame submitted the pointed-at work.
    //
    // The runtime half — an ungranted submitter is refused by the real lender —
    // ran above, before the grant committed, in this same lane and with no
    // sandbox, as did delegation and all of its negatives. What needs a real
    // RUN, and therefore a real VM, is the end-to-end proof that the executing
    // node's own broker composes that pointer without being told to:
    // `sched_pinned_run::a_delegated_run_draws_on_the_submitters_grant`.

    // And the lender serves no credential UPLOAD: its store is written by
    // `ducktape user cred add` on the owner's own disk. Sealing is not
    // authentication — `seal_pk` is on chain and served at `/attestation` — so a
    // route here would let any member replace the lent credential with their own
    // bearer, over exactly this overlay hop.
    let upload = rt.block_on(async {
        let gw = AirlockClient::remote("airlock.owner.duck".into(), via.clone());
        gw.upload_sealed_credential(
            &record.seal_pk,
            "owner-claude-1",
            airlock::wire::CredentialKind::Claude,
            &airlock::wire::CredentialPayload::Bearer {
                access_token: "ATTACKER-OWNS-THIS".into(),
            },
        )
        .await
    });
    assert!(
        upload.is_err(),
        "the lender must not serve a credential upload to the network: {upload:?}"
    );
}
