//! The frozen scenarios: every lifecycle the reachability machine is
//! expected to survive, run on the pure harness and pinned as a golden
//! event→effect trace under `fixtures/<scenario>.trace`. The trace IS the
//! specification — a behavior change is a PR that regenerates it
//! (`UPDATE_TRACES=1 cargo test -p netstack-scenarios`), and the review
//! reads the fixture diff. The assertions here are the coarse invariants a
//! reader should not have to find in the trace.
//!
//! Each scenario is a plain function over the [`Backend`] that builds its
//! nodes' machines; the `suite!` macro in the crate root turns the set into
//! one `#[test]` per scenario for each backend's test crate.

use crate::Backend;
use crate::harness::*;
use netstack_machine::{ReachabilityEvent, Resolution};

fn public_nodes(count: usize) -> Vec<NodeSpec> {
    (0..count)
        .map(|i| NodeSpec::public(10 * (i as u8 + 1)))
        .collect()
}

fn members(count: usize) -> Vec<usize> {
    (0..count).collect()
}

fn assert_converged(net: &Net, nodes: &[usize], epoch: u64, peers: usize) {
    for &node in nodes {
        assert!(
            converged(net, node, epoch, peers),
            "n{} verified and applied epoch {epoch} with {peers} peers",
            node + 1
        );
    }
}

/// How many times the node applied an epoch's tunnels.
fn applies(net: &Net, node: usize) -> usize {
    net.count(node, |e| {
        matches!(e, ReachabilityEvent::TunnelsApplied { .. })
    })
}

fn assert_no_peer_failure(net: &Net, nodes: &[usize]) {
    for &node in nodes {
        assert!(
            !net.saw(node, |e| matches!(e, ReachabilityEvent::PeerFailed { .. })),
            "n{} refused no peer",
            node + 1
        );
    }
}

fn readvertised(net: &Net, node: usize, peer: usize) -> bool {
    let key = net.key(peer);
    net.saw(
        node,
        |e| matches!(e, ReachabilityEvent::PeerReadvertised { peer, .. } if *peer == key),
    )
}

// ---------------------------------------------------------------- 1. boot

pub fn boot_two_members(backend: Backend) {
    let mut net = Net::new("boot_two_members", "net#boot", &public_nodes(2), backend);
    net.retarget_all(1, &members(2), &[], 1);
    net.run();
    assert_converged(&net, &members(2), 1, 1);
    assert_no_peer_failure(&net, &members(2));
    net.finish();
}

pub fn boot_three_members(backend: Backend) {
    let mut net = Net::new("boot_three_members", "net#boot", &public_nodes(3), backend);
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    assert_converged(&net, &members(3), 1, 2);
    assert_no_peer_failure(&net, &members(3));
    net.finish();
}

pub fn boot_five_members(backend: Backend) {
    let mut net = Net::new("boot_five_members", "net#boot", &public_nodes(5), backend);
    net.retarget_all(1, &members(5), &[], 1);
    net.run();
    assert_converged(&net, &members(5), 1, 4);
    assert_no_peer_failure(&net, &members(5));
    net.finish();
}

/// n2 and n3 share no link: everything between them — records, adverts,
/// the handshake triple — relays through the hub n1.
pub fn boot_relay_through_hub(backend: Backend) {
    let mut net = Net::new(
        "boot_relay_through_hub",
        "net#boot",
        &public_nodes(3),
        backend,
    )
    .isolated();
    net.connect(0, 1);
    net.connect(0, 2);
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    net.nudges(2);
    net.run();
    assert_converged(&net, &members(3), 1, 2);
    assert_no_peer_failure(&net, &members(3));
    net.finish();
}

// ------------------------------------------------------- 2/3. member restart

fn nated_mesh(scenario: &str, backend: Backend) -> Net {
    let specs = [
        NodeSpec::public(10),
        NodeSpec::endpoint_less(20),
        NodeSpec::public(30),
    ];
    let mut net = Net::new(scenario, "net#restart", &specs, backend)
        .with_coordinators()
        .with_persistence();
    for resolver in [0, 2] {
        net.resolve_answer(
            resolver,
            1,
            Some(Answer::ok(Resolution::Punched(addr(20, 40020)))),
        );
        net.rendezvous_answer(resolver, 1, Some(Answer::ok(addr(20, 40020))));
    }
    net
}

/// The NAT'd member restarts and its underlay mapping survived: the restored
/// base carries the boot gossip, the peers re-tunnel its fresh record in
/// place, and it adopts the mesh the peers had already locked — no cutover.
pub fn member_restart_mapping_kept(backend: Backend) {
    let mut net = nated_mesh("member_restart_mapping_kept", backend);
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    net.nudges(2);
    net.run();
    assert_converged(&net, &members(3), 1, 2);

    net.restart(1);
    net.retarget(1, 1, &members(3), &[], 40);
    net.run();
    // the peers answer the reborn member's phase-A on the heal cooldown, a
    // few nudge ticks after its fresh record arrives.
    net.nudges(6);
    net.run();

    assert!(net.saw(1, |e| matches!(
        e,
        ReachabilityEvent::MeshRestored { epoch: 1, .. }
    )));
    assert!(net.saw(1, |e| matches!(
        e,
        ReachabilityEvent::MeshAdopted { epoch: 1, .. }
    )));
    assert!(readvertised(&net, 0, 1) && readvertised(&net, 2, 1));
    net.finish();
}

/// The NAT'd member restarts behind a NEW mapping: the peers' remembered
/// punched address is dead. Its fresh record makes every peer rendezvous
/// it by identity again, and the re-tunnel carries the fresh mapping (the
/// trace's `WgApply` shows it); the reborn member adopts the locked mesh.
pub fn member_restart_mapping_lost(backend: Backend) {
    let mut net = nated_mesh("member_restart_mapping_lost", backend);
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    net.nudges(2);
    net.run();
    assert_converged(&net, &members(3), 1, 2);

    for resolver in [0, 2] {
        net.resolve_answer(
            resolver,
            1,
            Some(Answer::ok(Resolution::Punched(addr(20, 40021)))),
        );
        net.rendezvous_answer(resolver, 1, Some(Answer::ok(addr(20, 40021))));
    }
    net.restart(1);
    net.retarget(1, 1, &members(3), &[], 40);
    net.run();
    net.nudges(6);
    net.run();

    assert!(net.saw(1, |e| matches!(
        e,
        ReachabilityEvent::MeshRestored { epoch: 1, .. }
    )));
    assert!(net.saw(1, |e| matches!(
        e,
        ReachabilityEvent::MeshAdopted { epoch: 1, .. }
    )));
    assert!(readvertised(&net, 0, 1) && readvertised(&net, 2, 1));
    net.finish();
}

// ------------------------------------------------------------ 4. NAT rebind

/// A member's advertised endpoint moves mid-epoch (a rebind is a new life
/// with a new address): its fresh record re-advertises, and every peer
/// re-points the tunnel in place without a cutover.
pub fn nat_rebind_readvertises(backend: Backend) {
    let mut net = Net::new(
        "nat_rebind_readvertises",
        "net#rebind",
        &public_nodes(3),
        backend,
    );
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    assert_converged(&net, &members(3), 1, 2);

    net.set_advertised(1, 21);
    net.restart(1);
    net.retarget(1, 1, &members(3), &[], 40);
    net.run();
    net.nudges(2);
    net.run();

    assert!(readvertised(&net, 0, 1) && readvertised(&net, 2, 1));
    assert!(net.saw(1, |e| matches!(
        e,
        ReachabilityEvent::MeshAdopted { epoch: 1, .. }
    )));
    net.finish();
}

/// The same rebind, LIVE: the endpoint-less member's mapping moves while it
/// keeps running — no restart, no lost state, so nothing in the protocol
/// would ever look again. The host's own observation
/// (`Event::ReflexiveChanged`) is the only notice, and it makes the member
/// re-sign its record within the epoch: each peer runs the post-lock
/// re-advertisement path, rendezvouses it by identity again, and re-points
/// the tunnel at the NEW mapping in place. The member never adopts anything
/// — its view is the one it verified.
pub fn nat_rebind_live_readvertises(backend: Backend) {
    let mut net = nated_mesh("nat_rebind_live_readvertises", backend);
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    net.nudges(2);
    net.run();
    assert_converged(&net, &members(3), 1, 2);

    // the coordinator now answers with n2's new mapping — which is exactly
    // what n2's own host observed to fire the event below.
    for resolver in [0, 2] {
        net.resolve_answer(
            resolver,
            1,
            Some(Answer::ok(Resolution::Punched(addr(20, 40021)))),
        );
        net.rendezvous_answer(resolver, 1, Some(Answer::ok(addr(20, 40021))));
    }
    net.reflexive_changed(1, addr(20, 40021));
    net.run();
    net.nudges(2);
    net.run();

    assert!(readvertised(&net, 0, 1) && readvertised(&net, 2, 1));
    assert!(
        !net.saw(1, |e| matches!(e, ReachabilityEvent::MeshAdopted { .. })),
        "a live rebind re-signs inside the epoch this node itself verified"
    );
    assert_eq!(applies(&net, 1), 1, "the rebound member applied once");
    assert_no_peer_failure(&net, &members(3));
    net.finish();
}

// --------------------------------------------------------------- 5. cutover

/// A cutover over the SAME member set reconfigures the live interface in
/// place (an update, never a fresh bring-up), so unchanged peers keep
/// their sessions.
pub fn cutover_keeps_unchanged_peers(backend: Backend) {
    let mut net = Net::new(
        "cutover_keeps_unchanged_peers",
        "net#cutover",
        &public_nodes(3),
        backend,
    );
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    assert_converged(&net, &members(3), 1, 2);

    net.view_tick_all(90);
    net.retarget_all(2, &members(3), &[], 100);
    net.run();
    assert_converged(&net, &members(3), 2, 2);
    assert_no_peer_failure(&net, &members(3));
    net.finish();
}

/// A cutover that drops a member: the departed node tears its interface
/// down on the way out, the survivors re-verify a reduced mesh, and
/// traffic toward the departed node is simply gone.
pub fn cutover_to_reduced_mesh(backend: Backend) {
    let mut net = Net::new(
        "cutover_to_reduced_mesh",
        "net#cutover",
        &public_nodes(3),
        backend,
    );
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    assert_converged(&net, &members(3), 1, 2);

    net.shutdown(2);
    net.retarget_all(2, &members(2), &[], 100);
    net.run();
    assert_converged(&net, &members(2), 2, 1);
    net.finish();
}

// ----------------------------------------------------- 6. coordinator dark

/// Coordinators are configured but answer nothing: every resolve fails
/// honestly, assembly proceeds on advertised endpoints, and the by-identity
/// rendezvous sweep for the endpoint-less member backs off and caps.
pub fn coordinator_dark(backend: Backend) {
    let specs = [
        NodeSpec::public(10),
        NodeSpec::endpoint_less(20),
        NodeSpec::public(30),
    ];
    let mut net = Net::new("coordinator_dark", "net#dark", &specs, backend).with_coordinators();
    for node in 0..3 {
        for peer in 0..3 {
            net.resolve_answer(node, peer, Some(Answer::err("coordinator unreachable")));
            net.rendezvous_answer(node, peer, Some(Answer::err("coordinator unreachable")));
        }
    }
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    net.nudges(12);
    net.run();
    assert_converged(&net, &members(3), 1, 2);
    net.finish();
}

// ------------------------------------------------------- 7. fan-out lost

/// Both members lose the other's FIRST record: nothing in the record path
/// can heal (neither side ever hears the other), so the nudge re-offer is
/// the only cure — and it is enough.
pub fn first_fanout_lost_both_sides(backend: Backend) {
    let mut net = Net::new(
        "first_fanout_lost_both_sides",
        "net#loss",
        &public_nodes(2),
        backend,
    );
    net.link(0, 1, Link::dropping(vec![Loss::Next(1)]));
    net.link(1, 0, Link::dropping(vec![Loss::Next(1)]));
    net.retarget_all(1, &members(2), &[], 1);
    net.run();
    assert!(
        !converged(&net, 0, 1, 1),
        "nothing converges while both records are lost"
    );

    net.nudges(3);
    net.run();
    assert_converged(&net, &members(2), 1, 1);
    net.finish();
}

/// A cutover lands while the two members cannot reach each other at all:
/// nothing converges for as long as the partition holds, and the first
/// nudge after the link returns carries the records across and assembles
/// the new epoch — no cutover replay needed.
pub fn partition_across_cutover_heals_on_reconnect(backend: Backend) {
    let mut net = Net::new(
        "partition_across_cutover_heals_on_reconnect",
        "net#loss",
        &public_nodes(2),
        backend,
    );
    net.retarget_all(1, &members(2), &[], 1);
    net.run();
    assert_converged(&net, &members(2), 1, 1);

    net.partition(0, 1);
    net.view_tick_all(90);
    net.retarget_all(2, &members(2), &[], 100);
    net.run();
    net.nudges(2);
    net.run();
    assert!(
        !converged(&net, 0, 2, 1),
        "nothing converges across the partition"
    );

    net.reconnect(0, 1);
    net.nudges(3);
    net.run();
    assert_converged(&net, &members(2), 2, 1);
    assert_no_peer_failure(&net, &members(2));
    net.finish();
}

// --------------------------------------------------- 8. handshake lost

/// Each handshake message is lost once, at every stage, in both
/// directions: the nudge re-offers the STORED message verbatim — the trace
/// shows the same nonce each time — and the replay cache never sees a
/// nonce twice.
pub fn handshake_lost_at_each_stage(backend: Backend) {
    let lossy = || {
        Link::dropping(vec![
            Loss::Kind(MsgKind::Request, 1),
            Loss::Kind(MsgKind::Response, 1),
            Loss::Kind(MsgKind::Ack, 1),
        ])
    };
    let mut net = Net::new(
        "handshake_lost_at_each_stage",
        "net#loss",
        &public_nodes(2),
        backend,
    );
    net.link(0, 1, lossy());
    net.link(1, 0, lossy());
    net.retarget_all(1, &members(2), &[], 1);
    net.run();
    net.nudges(6);
    net.run();
    assert_converged(&net, &members(2), 1, 1);
    assert_no_peer_failure(&net, &members(2));
    net.finish();
}

/// Every message arrives twice: gossip dedups by nonce, handshakes by hash,
/// and the mesh converges exactly as it does over a clean link.
pub fn duplicated_delivery_is_tolerated(backend: Backend) {
    let mut net = Net::new(
        "duplicated_delivery_is_tolerated",
        "net#loss",
        &public_nodes(2),
        backend,
    );
    net.link(0, 1, Link::duplicating());
    net.link(1, 0, Link::duplicating());
    net.retarget_all(1, &members(2), &[], 1);
    net.run();
    assert_converged(&net, &members(2), 1, 1);
    assert_no_peer_failure(&net, &members(2));
    net.finish();
}

// ------------------------------------------------ 9. standby pre-warm

/// A standby's tunnels exist on both sides BEFORE its activation; the
/// promotion cutover then folds it into the verified mesh without tearing
/// anything down.
pub fn standby_prewarm_then_promotion(backend: Backend) {
    let mut net = Net::new(
        "standby_prewarm_then_promotion",
        "net#standby",
        &public_nodes(3),
        backend,
    );
    net.retarget_all(1, &[0, 1], &[2], 1);
    net.run();
    net.nudges(2);
    net.run();
    assert_converged(&net, &[0, 1], 1, 1);
    for member in [0, 1] {
        assert!(
            net.saw(member, |e| matches!(
                e,
                ReachabilityEvent::StandbyTunnelsApplied { epoch: 1, .. }
            )),
            "n{} pre-warmed the standby",
            member + 1
        );
    }
    assert!(net.saw(2, |e| matches!(
        e,
        ReachabilityEvent::StandbyTunnelsApplied { epoch: 1, .. }
    )));

    net.retarget_all(2, &members(3), &[], 100);
    net.run();
    net.nudges(2);
    net.run();
    assert_converged(&net, &members(3), 2, 2);
    net.finish();
}

/// A resident behind NAT — its record advertises no endpoint, so only it
/// can initiate — survives an epoch cutover that keeps it a resident. The
/// cutover arrives while it is cut off: the only transport it could
/// re-offer its record over rides the very tunnel the cutover rebuilds, so
/// a member that rebuilt its pre-warm layer from nothing would leave the
/// resident's handshake retries landing on an interface that no longer
/// knows its key, for good.
pub fn cutover_keeps_endpoint_less_standby(backend: Backend) {
    let specs = [
        NodeSpec::public(10),
        NodeSpec::public(20),
        NodeSpec::endpoint_less(30),
    ];
    let mut net = Net::new(
        "cutover_keeps_endpoint_less_standby",
        "net#natstandby",
        &specs,
        backend,
    );
    net.retarget_all(1, &[0, 1], &[2], 1);
    net.run();
    net.nudges(2);
    net.run();
    for member in [0, 1] {
        assert!(
            net.tunneled(member, 2),
            "n{} pre-warmed the endpoint-less resident",
            member + 1
        );
    }

    net.partition(0, 2);
    net.partition(1, 2);
    net.retarget_all(2, &[0, 1], &[2], 100);
    net.run();
    net.nudges(2);
    net.run();
    assert_converged(&net, &[0, 1], 2, 1);
    for member in [0, 1] {
        assert!(
            net.tunneled(member, 2),
            "n{} kept the endpoint-less resident's tunnel across the cutover",
            member + 1
        );
    }
    net.finish();
}

// ------------------------------------------------ 10. slow resolver

/// One resolve never answers: the rest of the mesh assembles regardless,
/// and a join-window install issued mid-stall is answered at once.
pub fn slow_resolver_does_not_stall(backend: Backend) {
    let mut net = Net::new(
        "slow_resolver_does_not_stall",
        "net#stall",
        &public_nodes(4),
        backend,
    );
    net.resolve_answer(0, 1, None);
    net.retarget_all(1, &members(3), &[], 1);
    net.run();
    assert_converged(&net, &members(3), 1, 2);

    let token = net.install_invite_peer(0, 3, addr(40, 51820));
    net.run();
    let installed = net.key(3);
    assert!(net.saw(
        0,
        |e| matches!(e, ReachabilityEvent::InvitePeerInstalled { peer, .. } if *peer == installed)
    ));
    assert_eq!(net.reply(token), Some(&Reply::Install(Ok(()))));
    net.finish();
}

// ---------------------------------------------------------- 11. join

/// The direct invite: the inviter merges the joiner as a join-window peer
/// onto its live mesh interface; the joiner, with no epoch at all, brings
/// its interface up with the inviter alone.
pub fn join_direct_invite(backend: Backend) {
    let mut net = Net::new("join_direct_invite", "net#join", &public_nodes(3), backend);
    net.retarget_all(1, &members(2), &[], 1);
    net.run();
    assert_converged(&net, &members(2), 1, 1);

    net.start(2);
    let inviter_side = net.install_invite_peer(0, 2, addr(30, 51820));
    let joiner_side = net.install_invite_peer(2, 0, addr(10, 51820));
    net.run();
    let joiner = net.key(2);
    let inviter = net.key(0);
    assert!(net.saw(
        0,
        |e| matches!(e, ReachabilityEvent::InvitePeerInstalled { peer, .. } if *peer == joiner)
    ));
    assert!(net.saw(
        2,
        |e| matches!(e, ReachabilityEvent::InvitePeerInstalled { peer, .. } if *peer == inviter)
    ));
    assert_eq!(net.reply(inviter_side), Some(&Reply::Install(Ok(()))));
    assert_eq!(net.reply(joiner_side), Some(&Reply::Install(Ok(()))));
    net.finish();
}

/// The coordinated invite: the joiner rendezvouses its inviter by identity,
/// installs it, sends the intro over the punched socket and awaits the ack
/// — once answered, once timing out.
pub fn join_coordinated_invite(backend: Backend) {
    let mut net = Net::new(
        "join_coordinated_invite",
        "net#join",
        &public_nodes(3),
        backend,
    )
    .with_coordinators();
    net.retarget_all(1, &members(2), &[], 1);
    net.run();

    net.start(2);
    net.rendezvous_answer(2, 0, Some(Answer::ok(addr(10, 40010))));
    net.datagram_reply(2, Answer::ok(b"ack".to_vec()));
    let answered = net.bootstrap_coordinated_invite(2, 0, b"intro".to_vec());
    net.run();

    net.datagram_reply(
        2,
        Answer {
            outcome: Err("intro ack timed out".into()),
            latency_ms: 2_000,
        },
    );
    let timed_out = net.bootstrap_coordinated_invite(2, 0, b"intro again".to_vec());
    net.run();

    let inviter = net.key(0);
    assert!(net.saw(
        2,
        |e| matches!(e, ReachabilityEvent::InvitePeerInstalled { peer, .. } if *peer == inviter)
    ));
    assert_eq!(
        net.reply(answered),
        Some(&Reply::Intro(Ok(b"ack".to_vec())))
    );
    assert!(matches!(net.reply(timed_out), Some(Reply::Intro(Err(_)))));
    net.finish();
}

// ----------------------------------------------- 12. backend swap mid-epoch

/// The machine behind a node is swapped for one restored from its
/// snapshot at three points of an epoch's life — while records and adverts
/// are still crossing, with handshake requests on the wire, and after the
/// apply — and the epoch neither restarts nor re-applies: one apply per
/// node, exactly as with no swap at all, and the next cutover runs on the
/// swapped machines like any other. The trace pins every snapshot's
/// digest, so every backend, and every native/guest crossing, must take
/// the same bytes from the same state.
pub fn backend_swap_mid_epoch(backend: Backend) {
    let mut net = Net::new(
        "backend_swap_mid_epoch",
        "net#swap",
        &public_nodes(3),
        backend,
    );
    net.retarget_all(1, &members(3), &[], 1);
    // records and adverts are still crossing: nothing verified yet.
    net.run_until(T0_MS + 2 * LINK_DELAY_MS + 5);
    net.swap(0);
    // the view verified and the handshake requests are on the wire.
    net.run_until(T0_MS + 3 * LINK_DELAY_MS + 5);
    for node in members(3) {
        assert_eq!(applies(&net, node), 0, "n{}: nothing applied yet", node + 1);
    }
    net.swap(1);
    net.run();
    assert_converged(&net, &members(3), 1, 2);
    assert_no_peer_failure(&net, &members(3));
    for node in members(3) {
        assert_eq!(
            applies(&net, node),
            1,
            "n{}: one apply for the epoch",
            node + 1
        );
    }

    // a swap of a settled node changes nothing the nudges can see.
    net.swap(2);
    net.nudges(3);
    net.run();
    for node in members(3) {
        assert_eq!(applies(&net, node), 1, "n{}: still one apply", node + 1);
    }

    // the swapped machines carry the next cutover like any other.
    net.shutdown(2);
    net.retarget_all(2, &members(2), &[], 100);
    net.run();
    assert_converged(&net, &members(2), 2, 1);
    for node in members(2) {
        assert_eq!(applies(&net, node), 2, "n{}: one apply per epoch", node + 1);
    }
    net.finish();
}
