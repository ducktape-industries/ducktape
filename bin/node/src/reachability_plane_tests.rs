//! wiring-semantics tests for the inviter-side intro handler
//! (`reachability_plane::handle_intro`): install-before-ack ordering and the
//! no-install/no-ack refusal of junk. the intro codec is covered by
//! `join_gate::tests` and the sealed-envelope crypto by `reachability::seal::tests`
//! — so these inject an IDENTITY `open` (the doorbell's open is DI) and drive
//! the post-open verify/install/ack path with plaintext bundles.

/// the injected opener for these unit tests: the seal roundtrip is proven in
/// `reachability::seal`, so here the "sealed" bytes ARE the plaintext bundle.
fn open_identity(sealed: &[u8]) -> Result<Vec<u8>, String> {
    Ok(sealed.to_vec())
}

use std::sync::{Arc, Mutex};

use commonware_cryptography::{Signer as _, ed25519};

use crate::config::mint_invite_token;
use crate::join_gate;
use crate::reachability_plane::{IntroPath, handle_intro};

const BINDING: &[u8] = b"net#00000000@feedface";

fn src() -> std::net::SocketAddr {
    "127.0.0.1:4242".parse().unwrap()
}

/// a real joiner-side WireGuard keypair: post-verify acks come back SEALED to
/// the intro's announced WG key, so the tests must hold its secret to read
/// them.
fn joiner_wg_keypair() -> reachability::WireGuardKeypair {
    let dir = tempfile::tempdir().unwrap();
    reachability::WireGuardKeypair::load_or_generate(&dir.path().join("wg.key"))
        .unwrap()
        .0
}

/// a valid encoded intro plus the joiner identity and WG keypair it binds.
fn intro_bytes() -> (Vec<u8>, ed25519::PublicKey, reachability::WireGuardKeypair) {
    let issuer = ed25519::PrivateKey::from_seed(1);
    let joiner = ed25519::PrivateKey::from_seed(2);
    let token = mint_invite_token(&issuer, BINDING, u64::MAX);
    let wg = joiner_wg_keypair();
    let msg = join_gate::intro_request(
        &joiner,
        BINDING,
        &token,
        wg.public_key().0,
        nat_traversal::now_secs(),
    );
    (join_gate::encode_intro(&msg), joiner.public_key(), wg)
}

/// open a post-verify (SEALED) ack with the joiner's WG secret and decode it.
fn open_sealed_ack(wg: &reachability::WireGuardKeypair, sealed: &[u8]) -> join_gate::IntroAck {
    let opened = wg
        .open_sealed(sealed)
        .expect("sealed to the joiner's WG key");
    join_gate::decode_intro_ack(&opened).expect("a decodable ack")
}

#[tokio::test]
async fn a_valid_intro_installs_before_acking() {
    let (bytes, joiner_pk, wg) = intro_bytes();
    let wg_pub = wg.public_key().0;
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(8);
    let weak = cmd_tx.downgrade();
    // one shared step log: the fake plane pushes "install" when the command
    // arrives, the ack closure pushes "ack" — order proves ack never outruns
    // the settled install.
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::default();
    let acked: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();

    let plane_log = log.clone();
    let plane = tokio::spawn(async move {
        match cmd_rx.recv().await.expect("install command sent") {
            reachability::ReachabilityCommand::InstallInvitePeer {
                peer,
                wireguard_public_key,
                endpoint,
                reply,
            } => {
                assert_eq!(peer, joiner_pk);
                assert_eq!(wireguard_public_key.0, wg_pub);
                assert_eq!(endpoint, src());
                plane_log.lock().unwrap().push("install");
                reply.0.send(Ok(())).unwrap();
            }
            other => panic!("expected InstallInvitePeer, got {other:?}"),
        }
    });

    let ack_log = log.clone();
    let ack_store = acked.clone();
    let alive = handle_intro(
        &bytes,
        src(),
        BINDING,
        "test",
        IntroPath::Direct,
        &weak,
        open_identity,
        None,
        |bytes| {
            ack_log.lock().unwrap().push("ack");
            ack_store.lock().unwrap().push(bytes);
            async {}
        },
    )
    .await;
    assert!(alive);
    plane.await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        ["install", "ack"],
        "the install command lands (and settles) before the ack"
    );
    let acked = acked.lock().unwrap();
    // the post-install ack is SEALED to the joiner's WG key — no gate hook
    // (`None`) means the reply is `Installed`.
    let ack = open_sealed_ack(&wg, &acked[0]);
    assert!(matches!(ack.reply, join_gate::IntroReply::Installed));
}

#[tokio::test]
async fn junk_neither_installs_nor_acks() {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<reachability::ReachabilityCommand>(8);
    let weak = cmd_tx.downgrade();
    let acks = Arc::new(Mutex::new(0usize));
    let n = acks.clone();
    let alive = handle_intro(
        b"not an intro",
        src(),
        BINDING,
        "test",
        IntroPath::Direct,
        &weak,
        open_identity,
        None,
        |_| {
            *n.lock().unwrap() += 1;
            async {}
        },
    )
    .await;
    assert!(alive, "junk keeps the loop running");
    assert!(cmd_rx.try_recv().is_err(), "no install for junk");
    assert_eq!(*acks.lock().unwrap(), 0, "no ack for junk");
}

#[tokio::test]
async fn a_failed_verification_stays_silent() {
    // a well-formed intro minted for ANOTHER network: decodes, fails verify.
    // Its WG key is unproven, so the refusal is the member's log line alone —
    // nothing goes back on the wire.
    let (bytes, _, _) = intro_bytes();
    for path in [IntroPath::Direct, IntroPath::Coordinated] {
        let (cmd_tx, mut cmd_rx) =
            tokio::sync::mpsc::channel::<reachability::ReachabilityCommand>(8);
        let weak = cmd_tx.downgrade();
        let acked: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let store = acked.clone();
        let alive = handle_intro(
            &bytes,
            src(),
            b"other-net",
            "test",
            path,
            &weak,
            open_identity,
            None,
            |b| {
                store.lock().unwrap().push(b);
                async {}
            },
        )
        .await;
        assert!(alive);
        assert!(cmd_rx.try_recv().is_err(), "no install on failed verify");
        assert!(acked.lock().unwrap().is_empty());
    }
}

/// drive one intro through `handle_intro` with no gate and report what it
/// did: whether it installed a tunnel peer, and every ack it sent.
async fn refusal_of(bytes: &[u8], path: IntroPath) -> (bool, Vec<Vec<u8>>) {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<reachability::ReachabilityCommand>(8);
    let weak = cmd_tx.downgrade();
    let acked: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let store = acked.clone();
    let alive = handle_intro(
        bytes,
        src(),
        BINDING,
        "test",
        path,
        &weak,
        open_identity,
        None,
        |b| {
            store.lock().unwrap().push(b);
            async {}
        },
    )
    .await;
    assert!(alive);
    let installed = cmd_rx.try_recv().is_ok();
    let acks = acked.lock().unwrap().clone();
    (installed, acks)
}

/// the one ack a refused intro earns, opened with the joiner's WG secret.
fn refused_detail(wg: &reachability::WireGuardKeypair, acks: &[Vec<u8>]) -> String {
    let [ack] = acks else {
        panic!("expected exactly one ack, got {}", acks.len());
    };
    match open_sealed_ack(wg, ack).reply {
        join_gate::IntroReply::Refused { detail } => detail,
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[tokio::test]
async fn an_expired_token_neither_installs_nor_tunnels() {
    // a cryptographically VALID intro whose token expired: verify passes (the
    // expiry is signature-covered, not signature-breaking), but the member's
    // wall-clock gate must refuse the tunnel — this is the enforcement point,
    // since consensus_time is block height and the joiner's decode check only
    // stops honest joiners.
    let issuer = ed25519::PrivateKey::from_seed(1);
    let joiner = ed25519::PrivateKey::from_seed(2);
    let token = mint_invite_token(&issuer, BINDING, 1); // 1970 — long expired
    let wg = joiner_wg_keypair();
    let bytes = join_gate::encode_intro(&join_gate::intro_request(
        &joiner,
        BINDING,
        &token,
        wg.public_key().0,
        nat_traversal::now_secs(),
    ));
    // BOTH doorbells owe the joiner the reason: a coordinated joiner that
    // hears nothing waits out its whole window and blames the mesh.
    for path in [IntroPath::Direct, IntroPath::Coordinated] {
        let (installed, acks) = refusal_of(&bytes, path).await;
        assert!(!installed, "no tunnel install for an expired token");
        // the expiry gate runs POST-verify, so its refusal is sealed.
        let detail = refused_detail(&wg, &acks);
        assert!(detail.contains("expired"), "{detail}");
    }
}

#[tokio::test]
async fn a_stale_intro_is_told_its_clock_is_off() {
    // every signature verifies and only the signed issued-at is out of the
    // freshness bound: the WG key is proven, so the refusal is sealed to it
    // and names the clock — the one thing a fresh invite cannot fix.
    let issuer = ed25519::PrivateKey::from_seed(1);
    let joiner = ed25519::PrivateKey::from_seed(2);
    let token = mint_invite_token(&issuer, BINDING, u64::MAX);
    let wg = joiner_wg_keypair();
    let skew = join_gate::INTRO_FRESHNESS_SECS + 600;
    let bytes = join_gate::encode_intro(&join_gate::intro_request(
        &joiner,
        BINDING,
        &token,
        wg.public_key().0,
        nat_traversal::now_secs() - skew,
    ));
    for path in [IntroPath::Direct, IntroPath::Coordinated] {
        let (installed, acks) = refusal_of(&bytes, path).await;
        assert!(!installed, "no tunnel install for a stale intro");
        let detail = refused_detail(&wg, &acks);
        assert!(detail.starts_with(join_gate::INTRO_STALE), "{detail}");
        assert!(detail.contains("clock"), "{detail}");
    }
}

#[test]
fn a_sealed_intro_hides_the_token_and_opens_only_for_the_member() {
    // item 5 mule test: the joiner seals its first-contact intro to the
    // member's WireGuard X25519 key. An on-path observer on café wifi sees only
    // opaque bytes; only the member holding the matching secret can open it, and
    // the opened bundle verifies end to end. A DIFFERENT member cannot open it.
    let dir = tempfile::tempdir().unwrap();
    let member = reachability::WireGuardKeypair::load_or_generate(&dir.path().join("member.key"))
        .unwrap()
        .0;
    let attacker =
        reachability::WireGuardKeypair::load_or_generate(&dir.path().join("attacker.key"))
            .unwrap()
            .0;

    let (plaintext, joiner_pk, wg) = intro_bytes();
    let wg_pub = wg.public_key().0;
    let sealed = reachability::seal(&member.public_key().0, &plaintext);

    // confidentiality: the bearer token never appears in the clear — the sealed
    // datagram does not decode as an intro, and a different key cannot open it.
    assert!(
        join_gate::decode_intro(&sealed).is_err(),
        "the sealed datagram must not be a cleartext intro an observer can read"
    );
    assert!(
        attacker.open_sealed(&sealed).is_err(),
        "only the member the intro was sealed to may open it"
    );

    // the member opens it and the bundle verifies exactly as before.
    let opened = member
        .open_sealed(&sealed)
        .expect("the member opens its own sealed intro");
    let msg = join_gate::decode_intro(&opened).expect("decodes after open");
    let verified = join_gate::verify_intro(&msg, BINDING, nat_traversal::now_secs())
        .expect("verifies end to end");
    assert_eq!(verified.joiner, joiner_pk);
    assert_eq!(verified.wg_public_key, wg_pub);
}

#[tokio::test]
async fn a_dead_plane_channel_stops_the_loop() {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<reachability::ReachabilityCommand>(8);
    let weak = cmd_tx.downgrade();
    drop(cmd_tx);
    drop(cmd_rx);
    let (bytes, _, _) = intro_bytes();
    let alive = handle_intro(
        &bytes,
        src(),
        BINDING,
        "test",
        IntroPath::Direct,
        &weak,
        open_identity,
        None,
        |_| async {},
    )
    .await;
    assert!(!alive, "a gone command channel tells the caller to exit");
}

#[tokio::test]
async fn a_gated_intro_forwards_once_and_answers_settled_outcomes() {
    // the member-side gate seam: the FIRST verified intro
    // forwards a GateForward to the run loop and acks sealed `Installed`;
    // once the loop writes an outcome into the shared map, a retransmit is
    // answered with THAT (sealed), without forwarding again.
    let (bytes, joiner_pk, wg) = intro_bytes();
    let (fwd_tx, mut fwd_rx) = tokio::sync::mpsc::channel::<join_gate::GateForward>(8);
    let outcomes: crate::reachability_plane::GateOutcomes = Default::default();
    let hook = crate::reachability_plane::GateHook {
        forward: fwd_tx,
        outcomes: outcomes.clone(),
    };

    // a fake plane that settles every install immediately.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(8);
    let weak = cmd_tx.downgrade();
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            if let reachability::ReachabilityCommand::InstallInvitePeer { reply, .. } = cmd {
                let _ = reply.0.send(Ok(()));
            }
        }
    });

    let acked: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let ring = |acked: Arc<Mutex<Vec<Vec<u8>>>>| {
        let bytes = bytes.clone();
        let hook = hook.clone();
        let weak = weak.clone();
        async move {
            handle_intro(
                &bytes,
                src(),
                BINDING,
                "test",
                IntroPath::Direct,
                &weak,
                open_identity,
                Some(&hook),
                |b| {
                    acked.lock().unwrap().push(b);
                    async {}
                },
            )
            .await
        }
    };

    // round 1: forwarded + sealed Installed.
    assert!(ring(acked.clone()).await);
    let fwd = fwd_rx.try_recv().expect("the gate request is forwarded");
    assert_eq!(fwd.joiner, joiner_pk.as_ref().to_vec());
    let ack = open_sealed_ack(&wg, &acked.lock().unwrap()[0]);
    assert!(matches!(ack.reply, join_gate::IntroReply::Installed));

    // the loop settles the gate: the retransmit reads the outcome — sealed —
    // and does NOT forward again.
    crate::reachability_plane::insert_gate_outcome(
        &mut outcomes.lock().unwrap(),
        joiner_pk.as_ref().to_vec(),
        join_gate::IntroReply::Admitted {
            height: 12,
            cap: None,
        },
        std::time::SystemTime::now(),
    );
    assert!(ring(acked.clone()).await);
    assert!(
        fwd_rx.try_recv().is_err(),
        "a settled joiner is answered from the map, never re-forwarded"
    );
    let ack = open_sealed_ack(&wg, &acked.lock().unwrap()[1]);
    assert!(matches!(
        ack.reply,
        join_gate::IntroReply::Admitted {
            height: 12,
            cap: None
        }
    ));
}

/// The handshake sampler's memory: an idle lapse is not a dark peer.
///
/// boringtun reports a session's age or NOTHING, and "nothing" covers both
/// "never handshaked" and "the 180s session just expired". Reading it alone
/// called every healthy tunnel DARK for the ~20s between a lapse and the
/// keepalive that heals it — a warn per peer every REJECT_AFTER_TIME, on a
/// mesh that was working.
#[test]
fn an_idle_session_lapse_is_not_a_dark_peer() {
    use crate::reachability_plane::session_verdicts;
    use std::time::Duration;

    let peer: std::net::Ipv6Addr = "fd1f::1".parse().expect("ula");
    let never: std::net::Ipv6Addr = "fd1f::2".parse().expect("ula");
    let mut seen = std::collections::HashMap::new();
    let t0 = tokio::time::Instant::now();

    let live = session_verdicts(&mut seen, t0, &[(peer, Some(Duration::from_secs(170)))]);
    assert!(live[0].live, "a session with an age is live");

    // the session expires; the keepalive has not re-handshaked yet.
    let lapsed = session_verdicts(&mut seen, t0 + Duration::from_secs(22), &[(peer, None)]);
    assert!(lapsed[0].live, "a lapsed-but-healing session is not dark");
    assert_eq!(lapsed[0].no_session_for, Some(Duration::from_secs(22)));

    // ...but one that never comes back is exactly what this watch is for.
    let dark = session_verdicts(&mut seen, t0 + Duration::from_secs(181), &[(peer, None)]);
    assert!(
        !dark[0].live,
        "no session for a whole session generation is dark"
    );

    // a peer whose config applied and never handshaked at all is dark on sight.
    let unseen = session_verdicts(&mut seen, t0, &[(never, None)]);
    assert!(!unseen[0].live);
    assert_eq!(
        unseen[0].no_session_for, None,
        "never handshaked, so there is no lapse to measure"
    );
}

// ---------------------------------------------------------------------------
// `underlay_addr`: the IPv4 underlay's pick out of a hostname resolution.
// ---------------------------------------------------------------------------

use crate::reachability_plane::underlay_addr;

fn addr(s: &str) -> std::net::SocketAddr {
    s.parse().unwrap()
}

/// the NAT64 shape (macOS CLAT46 on an IPv6-only network): getaddrinfo lists
/// the synthesised `64:ff9b::` candidate FIRST, the real IPv4 record behind
/// it. the real-IPv4 underlay socket refuses the V6 with EINVAL, so the pick
/// must be the V4.
#[test]
fn nat64_synthesised_v6_first_still_picks_the_v4() {
    let picked = underlay_addr([addr("[64:ff9b::334f:42b8]:3478"), addr("51.79.66.184:3478")]);
    assert_eq!(picked, Some(addr("51.79.66.184:3478")));
}

#[test]
fn the_first_v4_wins_among_several() {
    let picked = underlay_addr([addr("203.0.113.1:3478"), addr("203.0.113.2:3478")]);
    assert_eq!(picked, Some(addr("203.0.113.1:3478")));
}

#[test]
fn a_v6_only_host_is_unreachable_for_the_v4_underlay() {
    assert_eq!(underlay_addr([addr("[2001:db8::1]:3478")]), None);
    assert_eq!(underlay_addr([]), None);
}

// ---------------------------------------------------------------------------
// `GateOutcomes`: a sweep ages entries out on the invite join window.
// ---------------------------------------------------------------------------

use crate::reachability_plane::{GateOutcomeMap, insert_gate_outcome, sweep_gate_outcomes};

fn admitted_at(height: u64) -> join_gate::IntroReply {
    join_gate::IntroReply::Admitted { height, cap: None }
}

#[test]
fn sweep_removes_expired_entries_and_keeps_fresh_ones() {
    let mut map: GateOutcomeMap = GateOutcomeMap::new();
    let t0 = std::time::SystemTime::UNIX_EPOCH;
    let window = std::time::Duration::from_millis(reachability::INVITE_JOIN_WINDOW_MS);

    let stale = b"stale-joiner".to_vec();
    let fresh = b"fresh-joiner".to_vec();
    insert_gate_outcome(&mut map, stale.clone(), admitted_at(1), t0);
    let now = t0 + window + std::time::Duration::from_millis(1);
    insert_gate_outcome(&mut map, fresh.clone(), admitted_at(2), now);

    sweep_gate_outcomes(&mut map, now, window);

    assert!(
        !map.contains_key(&stale),
        "an entry older than the join window is swept, Admitted or not"
    );
    assert!(map.contains_key(&fresh), "a fresh entry survives the sweep");
}

// ---------------------------------------------------------------------------
// `UnreachableLatch`: the "peer unreachable" warn (#1768) — a forever-retry
// loop latched like `noded::log::Latch` (first hit, then every Nth, carrying
// the count), but per-peer and reset on recovery.
// ---------------------------------------------------------------------------

use crate::reachability_plane::UnreachableLatch;

#[test]
fn a_peer_latches_first_then_every_nth_and_counts_the_rest() {
    let mut latch = UnreachableLatch::default();
    let peer = b"peer-a";
    // the first failure must be visible immediately, not on the 100th retry.
    assert_eq!(latch.hit(peer), Some(1));
    for _ in 2..100 {
        assert_eq!(latch.hit(peer), None, "no flood while still latched");
    }
    // ...and the count is what tells you it is WEDGED, not merely flaky.
    assert_eq!(latch.hit(peer), Some(100));

    // a distinct peer latches independently: one noisy peer must never mask
    // another's first warn.
    assert_eq!(latch.hit(b"peer-b"), Some(1));
}

#[test]
fn a_recovered_peer_gets_a_fresh_first_warn() {
    let mut latch = UnreachableLatch::default();
    let peer = b"peer-a";
    assert_eq!(latch.hit(peer), Some(1));
    assert_eq!(latch.hit(peer), None);

    // the peer is reachable again — its next failure is a fresh first-warn,
    // not silently folded into the old streak's count.
    latch.clear(peer);
    assert_eq!(latch.hit(peer), Some(1));
}
