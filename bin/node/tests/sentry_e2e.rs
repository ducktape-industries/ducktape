//! sentry-fronted entry over real sockets: a validator advertises ONLY a
//! transparent TCP forwarder (a "sentry") in front of it — its real listen
//! port is never handed to any peer — and an out-of-mesh joiner still enters,
//! syncs, and promotes entirely through that pipe.
//!
//! this is the tightest proof of the Phase-1 sentry pattern: a network of ONE
//! (`Cluster::new(&[0], &[0])`) has quorum 2-of-2 only after the friend joins,
//! so consensus finalizes an op founder<->friend ONLY if the joiner reached
//! node 0 — and the joiner can reach node 0 ONLY via the sentry (its config
//! lists members by KEY only; its sole address for node 0 is the sentry). A
//! connection counter on the pipe confirms the joiner's entry traffic actually
//! transited the forwarder.
//!
//! the sentry is pure `std::net`/`std::thread` (no new dependency, matching the
//! harness idiom): a `TcpListener` accept loop that, per connection, dials the
//! target and splices both directions with `std::io::copy`. it counts BRIDGED
//! CONNECTIONS, not bytes — the mesh connection is long-lived, so a byte tally
//! taken at connection close would race the assertion, whereas a connection
//! count is settled the moment a client is bridged.

mod common;

use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::Cluster;
use common::wire::tasks::{
    TaskMsg, TaskQuery, TaskReply, decode_task_reply, encode_task_msg, encode_task_query,
};

/// convergence budget: mesh formation + leader rotation are real-time on a
/// possibly-loaded CI core; polls exit early, so generosity is free.
const CONVERGE: Duration = Duration::from_secs(180);
/// budget for one submitted op to finalize and become readable elsewhere.
const FINALIZE: Duration = Duration::from_secs(60);

/// a create for `task_id`; NOT an upsert — `tasks` refuses a duplicate id
/// (`task_board.rs:77-80`). That rejection is ISOLATED, not fatal: the op's
/// stage rolls back and it is recorded `Rejected` while the block still seals
/// (`host/src/lib.rs:237`, `:283`). So a duplicate never applies and never
/// announces itself — `write_and_confirm` spins to its timeout, or passes
/// VACUOUSLY when the re-created title happens to match the surviving one.
/// Every call site here has to carry a fresh id.
fn task_create(task_id: &str, title: &str) -> Vec<u8> {
    encode_task_msg(&TaskMsg::CreateTask {
        task_id: task_id.into(),
        title: title.into(),
        owner: None,
    })
}

fn task_title(cluster: &Cluster, idx: usize, task_id: &str) -> Option<String> {
    let req = encode_task_query(&TaskQuery::Get {
        task_id: task_id.into(),
    });
    let reply = cluster.query(idx, "tasks", &req)?;
    match decode_task_reply(&reply) {
        Ok(TaskReply::Task(task)) => task.map(|t| t.title),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    common::hex(bytes)
}

/// a transparent TCP forwarder in front of `target`. returns its public listen
/// address and a counter of CLIENT CONNECTIONS bridged to `target` — a race-free
/// proof that dialers reached node 0 through the forwarder. pure
/// `std::net`/`std::thread`; each direction is spliced with `std::io::copy`.
///
/// the upstream `connect` is lazy (per client connection): node 0 is already up
/// by the time any client dials the sentry, but a transient connect error must
/// never panic the accept loop — the connection is simply dropped.
fn spawn_sentry(target: SocketAddr) -> (SocketAddr, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sentry listener");
    let sentry_addr = listener.local_addr().expect("sentry local addr");
    let conns = Arc::new(AtomicU64::new(0));
    let accept_conns = Arc::clone(&conns);

    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let client = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            let upstream = match TcpStream::connect(target) {
                Ok(s) => s,
                Err(_) => continue, // target transiently unreachable — drop this conn
            };
            // two handles per socket so each relay thread owns one read + one
            // write half of the full-duplex pipe. a dup(2) failure (fd pressure)
            // drops just this connection — it must NEVER unwind the accept loop
            // and take the listener down with it (per the doc invariant above).
            let (client_rx, upstream_rx) = match (client.try_clone(), upstream.try_clone()) {
                (Ok(c), Ok(u)) => (c, u),
                _ => continue,
            };
            // count only connections that actually bridge (both halves cloned).
            accept_conns.fetch_add(1, Ordering::SeqCst);

            // client -> upstream
            std::thread::spawn(move || {
                let mut src = client;
                let mut dst = upstream;
                let _ = std::io::copy(&mut src, &mut dst);
                let _ = dst.shutdown(Shutdown::Write);
            });
            // upstream -> client
            std::thread::spawn(move || {
                let mut src = upstream_rx;
                let mut dst = client_rx;
                let _ = std::io::copy(&mut src, &mut dst);
                let _ = dst.shutdown(Shutdown::Write);
            });
        }
    });

    (sentry_addr, conns)
}

#[test]
fn joiner_enters_through_a_sentry() {
    // a network of ONE, fronted by a transparent TCP sentry. node 0 is mesh +
    // quorum all by itself; it advertises only the sentry, and its real listen
    // port is never handed to any peer.
    let mut cluster = Cluster::new(&[0], &[0]);

    // node 0's real (private) listen addr — the port was pre-allocated in
    // `new()`, so it is known before the config is written at spawn time.
    let node0_listen: SocketAddr = format!("127.0.0.1:{}", cluster.p2p_ports[0])
        .parse()
        .expect("node 0 listen addr parses");
    let (sentry_addr, bridged) = spawn_sentry(node0_listen);

    // everyone reaches node 0 ONLY through the sentry: node 0 advertises the
    // sentry, and the joiner's bootstrap hint points at the sentry too. set
    // BOTH before `spawn(0)` — the config is generated at spawn time.
    cluster.advertised[0] = Some(sentry_addr.to_string());
    cluster.bootstrap_addr_override = Some(sentry_addr.to_string());

    cluster.spawn(0);
    cluster.wait_marker(0, "rpc listening on", Duration::from_secs(60));
    cluster.wait_marker(0, "converged root_hash=", CONVERGE);

    // the friend starts an out-of-mesh node whose ONLY entry point is the
    // sentry; it must PARK until admitted. an UNINVITED key is (correctly)
    // rejected by node 0's p2p bouncer BEFORE admission — node 0 does not yet
    // track it — so the pre-admission parked state is genuinely
    // "joining: redemption not landed yet (...); retrying", matched by the
    // generic "joining:" prefix exactly as the sibling invite_e2e does. (the joiner
    // cannot reach node 0 until `promote` (direct admission) runs, fronted; the
    // sentry is what carries the sync + votes AFTER admission.)
    let joiner = cluster.spawn_joiner(1);
    cluster.wait_marker(joiner, "joiner mode:", Duration::from_secs(60));
    cluster.wait_marker(joiner, "joining:", Duration::from_secs(60));

    // one command on the founder admits the friend.
    let friend_hex = hex(&Cluster::identity(1));
    let cfg = cluster.config_file(0);
    let (ok, out) = cluster.run_verb(&[
        "node",
        "member",
        "promote",
        &friend_hex,
        "--config",
        cfg.to_str().expect("utf-8 config path"),
    ]);
    assert!(ok, "promote failed:\n{out}");
    assert!(out.contains("admitted"), "unexpected verb output:\n{out}");

    // direct admission: ONE cutover seats the friend — epoch 1 stalls at
    // its base (quorum 2-of-2) until the friend arrives.
    cluster.wait_marker(0, "cutover complete: epoch 1", CONVERGE);

    // the parked node syncs the boundary and promotes — every byte of that
    // handshake + state-sync crossed the sentry pipe.
    cluster.wait_marker(joiner, "admitted at epoch 1", CONVERGE);
    cluster.wait_marker(joiner, "synced root_hash=", CONVERGE);
    // in-process seating: no reboot, so no `recovered root_hash=` — that line
    // belongs to the validator boot path, which a warm promotion skips.
    cluster.wait_marker(joiner, "promoted: validator at epoch 1", CONVERGE);

    // sanity check that node 0 was reachable through the forwarder at all — a
    // corroboration, not the proof. the real proof is structural + functional:
    // the joiner has NO non-sentry path (its config lists members by key only;
    // its sole address for node 0 is the bootstrap hint = the sentry), and it
    // just went parked -> admitted -> synced -> promoted with a live cross-read
    // below. the counter also tallies pre-admission park dials, so `> 0` is not
    // a standalone anti-bypass guard — it simply confirms the forwarder was
    // exercised end-to-end rather than sitting idle.
    let bridged_conns = bridged.load(Ordering::SeqCst);
    assert!(
        bridged_conns > 0,
        "no connection transited the sentry (bridged={bridged_conns}) — the joiner did not enter through it"
    );

    // consensus is live again: a 2-of-2 simplex finalizes nothing without both
    // validators, so an op that commits proves the friend — which ENTERED
    // through the sentry — is now voting. (steady-state votes may ride a direct
    // founder<->friend link once discovery learns the friend's own advertised
    // address; the sentry proof is the ENTRY path, the cross-reads below are
    // independent liveness.) an op submitted via the FOUNDER must become
    // readable via the FRIEND...
    cluster.submit(0, "tasks", &task_create("from-founder", "hello"));
    let value = cluster.await_committed(
        joiner,
        "founder's op to read on the friend",
        FINALIZE,
        || task_title(&cluster, joiner, "from-founder"),
    );
    assert_eq!(value, "hello");

    // ...and an op submitted via the FRIEND must read on the founder.
    cluster.submit(joiner, "tasks", &task_create("from-friend", "hi back"));
    let value = cluster.await_committed(0, "friend's op to read on the founder", FINALIZE, || {
        task_title(&cluster, 0, "from-friend")
    });
    assert_eq!(value, "hi back");

    // no fork: identical status root-hashes once both sides quiesce (the founder
    // stayed alive throughout — 2-of-2 quorum must not tear down mid-read). a
    // real fork never reconciles, so it fails this wait's budget.
    cluster.await_committed(
        0,
        "founder and promoted friend to agree on state",
        FINALIZE,
        || {
            let founder = cluster.status(0)["root_hash"].clone();
            let agreed = !founder.is_null() && founder == cluster.status(joiner)["root_hash"];
            agreed.then_some(())
        },
    );
}
