// The lane table on the sim medium: a stream or a datagram is served by the handler bound to its
// lane's name, at the id and cap the table gives it, and a table change reaches the next open and
// the next arrival while a stream already open runs on.

use std::sync::{Arc, Mutex};

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use halo::sim::{SimLink, SimNet};
use halo::{Class, Error, Lane, LaneId, Members, Paced, Pacer, Plane, Seat};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};

const LANE: &str = "lane";

struct Pair {
    serving: Plane<SimLink>,
    asking: Plane<SimLink>,
    server: PublicKey,
}

/// Two validators on one sim medium, each with an empty table.
fn pair() -> Pair {
    let server = PrivateKey::from_seed(1).public_key();
    let client = PrivateKey::from_seed(2).public_key();
    let members = Arc::new(Members::default());
    members.seat([server.clone(), client.clone()].map(|key| Seat {
        key,
        halo: "localhost:1".into(),
        validator: true,
    }));
    let net = SimNet::new();
    Pair {
        serving: Plane::new(
            Arc::new(net.join(server.clone())),
            members.clone(),
            Pacer::unlimited(),
        ),
        asking: Plane::new(Arc::new(net.join(client)), members, Pacer::unlimited()),
        server,
    }
}

fn lane(name: &str, id: LaneId, cap: Class, enabled: bool) -> Lane {
    Lane {
        name: name.into(),
        id,
        cap,
        enabled,
        pinned: false,
        since: 0,
    }
}

fn served(id: LaneId) -> Vec<Lane> {
    vec![lane(LANE, id, Class::Critical, true)]
}

/// Whether the serving side let the stream go: the asker reads its end, and it ends.
async fn let_go(stream: &mut Paced<DuplexStream>) -> bool {
    let mut byte = [0u8; 1];
    matches!(stream.read(&mut byte).await, Ok(0))
}

#[tokio::test]
async fn a_stream_is_handed_to_the_handler_bound_by_its_lanes_name() {
    let pair = pair();
    let table = vec![
        lane("other", 0, Class::Critical, true),
        lane(LANE, 1, Class::Critical, true),
    ];
    pair.serving.lanes(table.clone());
    pair.asking.lanes(table);
    let bound = [
        pair.serving.streams("other", 4).unwrap(),
        pair.serving.streams(LANE, 4).unwrap(),
    ];
    for (name, wanted) in [("other", 0), (LANE, 1)] {
        let wanting = pair.asking.streams(name, 4).unwrap();
        let _open = wanting
            .open(&pair.server, Class::Member, name.as_bytes())
            .await
            .unwrap();
        let handed = tokio::select! {
            inbound = bound[0].accept() => (0, inbound.unwrap().meta),
            inbound = bound[1].accept() => (1, inbound.unwrap().meta),
        };
        assert_eq!(handed, (wanted, name.as_bytes().to_vec()));
    }
}

#[tokio::test]
async fn a_stream_for_an_id_the_table_does_not_map_is_let_go() {
    let pair = pair();
    pair.asking.lanes(served(3));
    pair.serving.lanes(served(4));
    let _served = pair.serving.streams(LANE, 4).unwrap();
    let wanting = pair.asking.streams(LANE, 4).unwrap();
    let mut stream = wanting
        .open(&pair.server, Class::Member, b"")
        .await
        .unwrap();
    assert!(let_go(&mut stream).await);
}

#[tokio::test]
async fn a_paused_lane_refuses_an_open_and_lets_go_what_arrives() {
    let pair = pair();
    let paused = vec![lane(LANE, 0, Class::Critical, false)];
    pair.asking.lanes(paused.clone());
    pair.serving.lanes(paused);
    let bound = pair.serving.streams(LANE, 4).unwrap();
    let wanting = pair.asking.streams(LANE, 4).unwrap();
    let refused = wanting.open(&pair.server, Class::Member, b"").await;
    assert!(
        matches!(&refused, Err(Error::NoLane(name)) if name == LANE),
        "{:?}",
        refused.err()
    );

    pair.asking.lanes(served(0));
    let mut early = wanting
        .open(&pair.server, Class::Member, b"early")
        .await
        .unwrap();
    assert!(let_go(&mut early).await);

    pair.serving.lanes(served(0));
    let _resumed = wanting
        .open(&pair.server, Class::Member, b"resumed")
        .await
        .unwrap();
    let inbound = bound.accept().await.unwrap();
    assert_eq!(inbound.meta, b"resumed", "the paused arrival was not kept");
}

#[tokio::test]
async fn a_name_bound_before_the_table_lists_it_is_served_once_it_does() {
    let pair = pair();
    let bound = pair.serving.streams(LANE, 4).unwrap();
    pair.asking.lanes(served(0));
    let wanting = pair.asking.streams(LANE, 4).unwrap();
    let mut early = wanting
        .open(&pair.server, Class::Member, b"early")
        .await
        .unwrap();
    assert!(let_go(&mut early).await);

    pair.serving.lanes(served(0));
    let _listed = wanting
        .open(&pair.server, Class::Member, b"listed")
        .await
        .unwrap();
    assert_eq!(bound.accept().await.unwrap().meta, b"listed");
}

#[tokio::test]
async fn a_stream_open_when_its_lane_closes_runs_on() {
    let pair = pair();
    pair.asking.lanes(served(0));
    pair.serving.lanes(served(0));
    let bound = pair.serving.streams(LANE, 4).unwrap();
    let wanting = pair.asking.streams(LANE, 4).unwrap();
    let mut asked = wanting
        .open(&pair.server, Class::Member, b"")
        .await
        .unwrap();
    let mut answering = bound.accept().await.unwrap().stream;

    pair.asking.lanes(Vec::new());
    pair.serving.lanes(Vec::new());
    answering.write_all(b"still").await.unwrap();
    let mut answer = [0u8; 5];
    asked.read_exact(&mut answer).await.unwrap();
    assert_eq!(&answer, b"still");
    let closed = wanting.open(&pair.server, Class::Member, b"").await;
    assert!(matches!(closed, Err(Error::NoLane(_))));
}

#[tokio::test]
async fn a_stream_is_served_at_the_lower_of_what_it_asked_and_its_lanes_cap() {
    let pair = pair();
    let capped = vec![lane(LANE, 0, Class::Member, true)];
    pair.asking.lanes(capped.clone());
    pair.serving.lanes(capped);
    let bound = pair.serving.streams(LANE, 4).unwrap();
    let wanting = pair.asking.streams(LANE, 4).unwrap();
    for (asked, served) in [
        (Class::Critical, Class::Member),
        (Class::Client, Class::Client),
    ] {
        let opened = wanting.open(&pair.server, asked, b"").await.unwrap();
        assert_eq!(opened.class(), served, "paced at the asker, {asked:?}");
        let inbound = bound.accept().await.unwrap();
        assert_eq!(inbound.class, served, "served, {asked:?}");
        assert_eq!(inbound.stream.class(), served);
    }
}

#[tokio::test]
async fn a_datagram_is_handed_to_the_handler_bound_by_its_lanes_name() {
    let pair = pair();
    pair.asking.lanes(served(5));
    pair.serving.lanes(served(5));
    let unlisted = pair.asking.datagrams("unlisted", 4).unwrap();
    let refused = unlisted.send(&pair.server, b"nowhere").await;
    assert!(matches!(refused, Err(Error::NoLane(name)) if name == "unlisted"));

    let received = pair.serving.datagrams(LANE, 4).unwrap();
    let sending = pair.asking.datagrams(LANE, 4).unwrap();
    pair.asking.lanes(served(6));
    sending.send(&pair.server, b"unmapped").await.unwrap();
    pair.asking.lanes(served(5));
    sending.send(&pair.server, b"mapped").await.unwrap();
    let (_, payload) = received.recv().await;
    assert_eq!(
        payload, b"mapped",
        "a datagram for an id the table does not map is dropped"
    );
}

/// What `during` logs at warn and above on this thread, as text.
fn warned(during: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, during);
    String::from_utf8(captured.0.lock().unwrap().clone()).unwrap()
}

#[tokio::test]
async fn a_table_that_replaces_one_warns_for_each_served_name_nothing_binds() {
    let pair = pair();
    let table = vec![
        lane(LANE, 0, Class::Critical, true),
        lane("idle", 1, Class::Critical, true),
        lane("paused", 2, Class::Critical, false),
    ];
    let first = warned(|| pair.serving.lanes(table.clone()));
    assert_eq!(first, "", "the first table is taken before anything binds");

    let _bound = pair.serving.streams(LANE, 4).unwrap();
    let again = warned(|| pair.serving.lanes(table));
    let unbound: Vec<&str> = again
        .lines()
        .filter(|line| line.contains("halo_unbound"))
        .collect();
    assert_eq!(unbound.len(), 1, "{again}");
    assert!(unbound[0].contains("idle"), "{again}");

    let unlisted = warned(|| {
        pair.serving.datagrams("nowhere", 4).unwrap();
    });
    assert!(
        unlisted.contains("halo_unlisted") && unlisted.contains("nowhere"),
        "{unlisted}"
    );
}
