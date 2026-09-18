//! the automatic half of onboarding, end to end: minting the invite IS the
//! admission decision. a joiner holding a TOKENED invite delivers its pubkey
//! in its sealed first-contact intro, the receiving member submits the governance
//! `Redeem` op on its behalf — no approval verb, no human in the middle —
//! and the joiner comes up as a FULL NODE (observer standing: mesh +
//! statesync + a serving read surface). seating it in the QUORUM stays a
//! separate, deliberate act (`promote`), exercised at the end.

mod common;

use std::time::Duration;

use common::NetworkShapeCluster;
use commonware_cryptography::Signer as _;
use valset::{ValsetQuery, ValsetReply};

const CONVERGE: Duration = Duration::from_secs(180);

#[test]
fn a_tokened_join_redeems_itself_into_a_full_node() {
    let mut cluster = NetworkShapeCluster::new();

    cluster.init_founder("join-request");
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // the default invite carries the token — the admission capability.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend(&invite);
    assert_eq!(friend_key.len(), 64, "join prints the friend's pubkey hex");

    cluster.spawn(1);
    cluster.wait_marker(1, "joiner mode:", Duration::from_secs(60));

    // the joiner reaches the founder on its own (the join protocol first contact or the
    // announce fallback), and the founder redeems it — NO verb runs anywhere
    // in this window.
    cluster.wait_admitted(1, Duration::from_secs(90));
    cluster.wait_marker(0, "gate: redemption submitted for", Duration::from_secs(90));

    // the redemption lands in consensus state: the friend holds RESIDENT
    // standing (a full node), while the quorum still seats only the founder.
    let expected = vec![common::unhex(&friend_key)];
    cluster.await_committed(
        0,
        "the redemption to grant resident standing",
        CONVERGE,
        || {
            cluster
                .query(0, "valset", &valset::encode_query(&ValsetQuery::Residents))
                .and_then(|raw| valset::decode_reply(&raw).ok())
                .and_then(|r| match r {
                    ValsetReply::Residents(v) if v == expected => Some(()),
                    _ => None,
                })
        },
    );
    let validators = cluster
        .query(0, "valset", &valset::encode_query(&ValsetQuery::Validators))
        .and_then(|raw| valset::decode_reply(&raw).ok())
        .map(|r| match r {
            ValsetReply::Validators(v) => v,
            other => panic!("expected Validators, got {other:?}"),
        })
        .expect("valset validators readable");
    assert_eq!(
        validators.len(),
        1,
        "the quorum still seats ONLY the founder"
    );

    // the full node pre-syncs and serves — the whole point of the flow.
    cluster.wait_marker(1, "resident: pre-synced boundary", CONVERGE);

    // a second announce cannot double-admit: the nonce is spent, standing
    // already exists, and the founder's tracker drains once settled.
    let requests = cluster.join_requests();
    assert_eq!(
        requests.as_array().map(Vec::len),
        Some(0),
        "a settled redemption leaves the queue: {requests:?}"
    );

    // seating it in the quorum is a separate, deliberate act — the existing
    // promote verb over the standing the redemption granted. the redemption's
    // own grant cutover was epoch 1, so the promotion cuts over to epoch 2.
    let (ok, out) = cluster.run_promote(&friend_key);
    assert!(ok, "promote failed:\n{out}");
    cluster.wait_marker(0, "cutover complete: epoch 2", CONVERGE);
    cluster.wait_marker(1, "promoted: validator at epoch 2", CONVERGE);
}

/// A joiner seated by a validator promoted AFTER genesis gets a coordinator
/// cap that a private coordinator admits. The founder rotates out first, so
/// the promoted friend is the only validator left to seat the third party:
/// its cap's issuer is in no genesis set, and the coordinator admits the
/// third party's rendezvous only because it follows the network's CURRENT
/// validator set off a node.
#[test]
fn a_joiner_seated_by_a_promoted_validator_rendezvouses_through_a_private_coordinator() {
    let mut cluster = NetworkShapeCluster::new();

    cluster.init_founder("coord-follows-valset");
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    let founder_key =
        workspace_config::NetworkDescriptor::load(&cluster.founder_dir.join("network.toml"))
            .expect("the founder's descriptor")
            .validators
            .remove(0);

    // the friend joins and is promoted: a validator genesis never named.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend(&invite);
    cluster.spawn(1);
    cluster.wait_admitted(1, Duration::from_secs(90));
    let (ok, out) = cluster.run_promote(&friend_key);
    assert!(ok, "promote failed:\n{out}");
    cluster.wait_marker(1, "promoted: validator at epoch", CONVERGE);

    // the founder rotates out. The founder votes first so the surviving
    // friend executes the removal; the cutover halts the founder.
    let (ok, out) = cluster.run_membership_verb_as(0, "member remove", &founder_key);
    assert!(ok, "founder member remove ballot failed:\n{out}");
    let (ok, out) = cluster.run_membership_verb_as(1, "member remove", &founder_key);
    assert!(ok, "friend member remove ballot failed:\n{out}");
    cluster.wait_marker(0, "demoted from the validator set; halting", CONVERGE);
    cluster.wait_exit(0, CONVERGE);
    let only_the_friend = vec![common::unhex(&friend_key)];
    cluster.await_committed(1, "the friend to be the only validator", CONVERGE, || {
        cluster
            .query(1, "valset", &valset::encode_query(&ValsetQuery::Validators))
            .and_then(|raw| valset::decode_reply(&raw).ok())
            .and_then(|r| match r {
                ValsetReply::Validators(v) if v == only_the_friend => Some(()),
                _ => None,
            })
    });

    // a third party the friend invites — and, the founder gone, seats.
    let third = cluster.add_node();
    let blob = cluster.invite_from(1);
    let third_key = cluster.join(third, &blob);
    cluster.spawn(third);
    cluster.wait_marker(third, "coordinator cap delivered and saved", CONVERGE);
    let workspace = cluster.workspace(third);
    let cap = workspace_config::load_coord_cap(&workspace)
        .expect("the delivered cap reads back")
        .expect("the seat delivered a cap");
    assert_eq!(
        common::hex(cap.issuer.as_ref()),
        friend_key,
        "the promoted friend minted the cap"
    );
    let signer = workspace_config::load_identity(&workspace.join("identity.key"))
        .expect("the third party's identity");
    assert_eq!(common::hex(signer.public_key().as_ref()), third_key);
    let subject = nat_traversal::NodeKey(signer.public_key().as_ref().try_into().unwrap());

    // the private coordinator an operator runs for this network: pinned to
    // the founder's network.toml, following the friend's node.
    let founder_toml = cluster.founder_dir.join("network.toml");
    let args = [
        "--genesis-set".to_string(),
        founder_toml.to_str().expect("utf-8 path").to_string(),
        "--valset-node".to_string(),
        format!("http://127.0.0.1:{}", cluster.http_ports[1]),
    ];
    let policy = coordinator_bin::select_policy(&args).expect("a private policy");
    let node = coordinator_bin::valset_node(&args)
        .expect("a valid --valset-node")
        .expect("--valset-node is set");
    let nat_traversal::AuthPolicy::Private { genesis_set, live } = &policy else {
        panic!("--genesis-set selects the private policy");
    };
    assert!(
        !genesis_set.contains(&cap.issuer),
        "the issuer is no genesis validator"
    );
    // pinned to genesis alone, the coordinator refuses this cap: the defect.
    let now = nat_traversal::now_secs();
    let auth = nat_traversal::sign_authenticator(&signer, b"bind", now, Some(cap.clone()));
    assert_eq!(
        nat_traversal::verify_request(&policy, now, 30, subject, b"bind", &auth),
        Err(nat_traversal::AuthError::NotAdmitted),
        "a genesis-only coordinator refuses a promoted validator's cap"
    );

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        // one read of the friend's committed set over its open query lane...
        let read = coordinator_bin::refresh(&node, live)
            .await
            .expect("the friend's node serves its validator set");
        assert_eq!(common::hex(read[0].as_ref()), friend_key);

        // ...and the third party's own rendezvous, with its own key and the
        // cap it was delivered, is admitted.
        let coord_sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind the coordinator");
        let coord_addr = coord_sock.local_addr().expect("coordinator addr");
        tokio::spawn(nat_traversal::run_coordinator(
            nat_traversal::NatSocket::Owned(coord_sock),
            policy.clone(),
        ));
        let resolver =
            reachability::NatResolver::bind(subject, vec![coord_addr], (signer, Some(cap)))
                .await
                .expect("the rendezvous socket binds");
        let mut status = resolver.status().expect("the resolver has a coordinator");
        tokio::time::timeout(CONVERGE, async {
            while !matches!(
                *status.borrow_and_update(),
                reachability::RendezvousStatus::Ready { .. }
            ) {
                status.changed().await.expect("the establish task is alive");
            }
        })
        .await
        .expect("the private coordinator admits the third party's rendezvous");
    });
}

#[test]
fn a_spent_invite_is_refused_loudly_on_both_ends() {
    let mut cluster = NetworkShapeCluster::new();

    cluster.init_founder("spent-invite");
    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));

    // first redeemer: the normal flow, driven to a COMMITTED redemption so
    // the nonce is durably spent before anyone reuses the blob.
    let invite = cluster.invite();
    let friend_key = cluster.join_friend(&invite);
    cluster.spawn(1);
    cluster.wait_marker(0, "gate: redemption submitted for", Duration::from_secs(90));
    let expected = vec![common::unhex(&friend_key)];
    cluster.await_committed(
        0,
        "the redemption to grant resident standing",
        CONVERGE,
        || {
            cluster
                .query(0, "valset", &valset::encode_query(&ValsetQuery::Residents))
                .and_then(|raw| valset::decode_reply(&raw).ok())
                .and_then(|r| match r {
                    ValsetReply::Residents(v) if v == expected => Some(()),
                    _ => None,
                })
        },
    );

    // second redeemer: the SAME blob under a FRESH identity — the shared-blob
    // mistake. a bearer invite mints the workspace locally without
    // complaint — there is no targeted key for the CLI to check — and the
    // single-use invariant lands TERMINALLY at first contact: the founder's
    // gate sees the spent nonce and refuses permanently, and the joiner stops
    // loudly instead of parking forever.
    cluster.kill(1);
    std::fs::remove_dir_all(&cluster.friend_dir).expect("wipe first redeemer");
    let out = cluster.try_join_friend(&invite);
    assert!(
        out.status.success(),
        "a bearer join mints the workspace locally; the refusal is at first contact:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    cluster.spawn(1);
    cluster.wait_marker(
        0,
        "ALREADY-REDEEMED invite",
        std::time::Duration::from_secs(90),
    );
    cluster.wait_marker(1, "join gate refused", std::time::Duration::from_secs(90));
    // the refusal is terminal: the joiner exits instead of spinning.
    cluster.wait_exit(1, std::time::Duration::from_secs(60));
    let _ = friend_key;
}
