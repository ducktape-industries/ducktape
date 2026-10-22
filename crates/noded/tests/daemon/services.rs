// Egress a node or a box serves.

use super::*;

const PONG: &str = "pong";

async fn upstream() -> SocketAddr {
    let listener = ::tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let at = listener.local_addr().unwrap();
    let router = axum::Router::new().route("/ping", axum::routing::get(|| async { PONG }));
    ::tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    at
}

fn write_egress(seat: &Seat, text: &str) {
    std::fs::write(seat.workspace.dir().join("egress.toml"), text).unwrap();
}

async fn reload(seat: &Seat, client: &Client) -> noded::Result<()> {
    let anchor = client.status().await.unwrap().anchor();
    client
        .admin(seat.admin_frame(anchor, Admin::EgressReload))
        .await
}

async fn pinged(
    client: &Client,
    changes: &mut (impl futures::Stream<Item = noded::Result<Change>> + Unpin),
    service: &str,
) {
    let Outcome::Applied { output } = request(client, service).await else {
        panic!("the request for {service} was not taken");
    };
    let id: u64 = abi::decode(&output).unwrap();
    let egress::Answer::Output(bytes) = answered(client, changes, id).await else {
        panic!("{service} answered request {id} with a failure");
    };
    let response: egress::http::Response = abi::decode(&bytes).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, PONG.as_bytes());
}

#[test]
fn a_running_node_serves_what_each_egress_reload_reads_and_keeps_its_services_on_a_bad_file() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let echo = upstream().await;
        let mut changes = client.changes("egress").await.unwrap();
        let unserved = request(&client, "echo").await;
        assert!(
            matches!(&unserved, Outcome::Rejected(refusal) if refusal.reason == reason::NOT_FOUND),
            "{unserved:?}"
        );

        write_egress(
            &seat,
            &format!("[services.echo]\nbase = \"http://{echo}\"\n"),
        );
        reload(&seat, &client).await.unwrap();
        recorded(&client, &mut changes, &seat.key(), &["echo"]).await;
        pinged(&client, &mut changes, "echo").await;

        write_egress(&seat, "relya = true\n");
        let Err(noded::Error::Refused(refusal)) = reload(&seat, &client).await else {
            panic!("a file that does not parse is refused");
        };
        assert_eq!(refusal.reason, reason::INVALID_INPUT);
        pinged(&client, &mut changes, "echo").await;

        write_egress(&seat, "");
        reload(&seat, &client).await.unwrap();
        recorded(&client, &mut changes, &seat.key(), &[]).await;
        let gone = request(&client, "echo").await;
        assert!(
            matches!(&gone, Outcome::Rejected(refusal) if refusal.reason == reason::NOT_FOUND),
            "{gone:?}"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

const GRANTEE_SEED: u64 = 31;

async fn set_egress(
    client: &Client,
    key: &ed25519::PrivateKey,
    text: &str,
    dry: bool,
) -> noded::Result<EgressPlan> {
    let anchor = client.status().await.unwrap().anchor();
    let asked = EgressSet {
        text: text.to_owned(),
        dry,
    };
    let frame = Frame::sign(
        key,
        NETWORK.as_bytes(),
        anchor,
        None,
        noded::wire::EGRESS,
        abi::encode(&asked),
    );
    client.egress_set(frame.encode()).await
}

fn served_file(seat: &Seat) -> Option<String> {
    std::fs::read_to_string(seat.workspace.dir().join("egress.toml")).ok()
}

#[test]
fn an_egress_set_from_a_granted_key_serves_its_text_after_a_dry_run_that_says_whether_each_base_answers()
 {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let grantee = ed25519::PrivateKey::from_seed(GRANTEE_SEED);

    client_runtime().block_on(async {
        let echo = upstream().await;
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut changes = client.changes("egress").await.unwrap();
        let both = format!(
            "[services.echo]\nbase = \"http://{echo}\"\n[services.gone]\nbase = \"http://{closed}\"\n"
        );
        let Err(noded::Error::Refused(refusal)) = set_egress(&client, &grantee, &both, true).await
        else {
            panic!("a key egress-grants does not list is refused");
        };
        assert_eq!(refusal.reason, reason::UNAUTHORIZED);
        std::fs::write(
            seat.workspace.dir().join("egress-grants"),
            format!("{}\n", hex::encode(grantee.public_key().as_ref())),
        )
        .unwrap();

        let dry = set_egress(&client, &grantee, &both, true).await.unwrap();
        assert_eq!((dry.served, dry.added, dry.removed, dry.changed), (2, 2, 0, 0));
        assert_eq!(dry.reach[0], ("echo".to_owned(), Reached::Answered(404)));
        assert!(
            matches!(&dry.reach[1], (name, Reached::Failed(_)) if name == "gone"),
            "{:?}",
            dry.reach
        );
        assert_eq!(served_file(&seat), None);
        let unserved = request(&client, "echo").await;
        assert!(
            matches!(&unserved, Outcome::Rejected(refusal) if refusal.reason == reason::NOT_FOUND),
            "{unserved:?}"
        );

        let stranger = ed25519::PrivateKey::from_seed(PUTTER_SEED);
        let Err(noded::Error::Refused(refusal)) = set_egress(&client, &stranger, &both, false).await
        else {
            panic!("a key egress-grants does not list is refused");
        };
        assert_eq!(refusal.reason, reason::UNAUTHORIZED);

        let seed = format!(
            "[services.echo]\nbase = \"http://{echo}\"\nsecrets = {{ x-seed = {{ file = \"identity.key\" }} }}\n"
        );
        let variable = format!(
            "[services.echo]\nbase = \"http://{echo}\"\nsecrets = {{ x-api-key = \"PATH\" }}\n"
        );
        let private = "http = true\nprivate = true\n";
        let refusals = [
            (&grantee, seed.as_str(), reason::INVALID_INPUT, "echo carries secrets"),
            (&seat.identity, seed.as_str(), reason::INVALID_INPUT, "echo carries secrets"),
            (&grantee, variable.as_str(), reason::INVALID_INPUT, "echo carries secrets"),
            (&seat.identity, variable.as_str(), reason::INVALID_INPUT, "echo carries secrets"),
            (&grantee, private, reason::UNAUTHORIZED, "private"),
        ];
        for (key, text, refused, named) in refusals {
            let Err(noded::Error::Refused(refusal)) = set_egress(&client, key, text, true).await
            else {
                panic!("a set naming {named} is refused");
            };
            assert_eq!(refusal.reason, refused);
            assert!(refusal.sentence.contains(named), "{}", refusal.sentence);
        }
        let own = set_egress(&client, &seat.identity, private, true).await.unwrap();
        assert_eq!(own.served, 1);

        let echo_only = format!("[services.echo]\nbase = \"http://{echo}\"\n");
        let set = set_egress(&client, &grantee, &echo_only, false).await.unwrap();
        assert_eq!((set.served, set.added), (1, 1));
        recorded(&client, &mut changes, &seat.key(), &["echo"]).await;
        pinged(&client, &mut changes, "echo").await;
        assert_eq!(served_file(&seat), Some(echo_only.clone()));

        let Err(noded::Error::Refused(refusal)) =
            set_egress(&client, &grantee, "relya = true\n", false).await
        else {
            panic!("a file that does not parse is refused");
        };
        assert_eq!(refusal.reason, reason::INVALID_INPUT);
        pinged(&client, &mut changes, "echo").await;
        assert_eq!(served_file(&seat), Some(echo_only.clone()));

        std::fs::write(seat.workspace.dir().join("egress-grants"), "zz\n").unwrap();
        let Err(noded::Error::Refused(refusal)) =
            set_egress(&client, &grantee, &echo_only, true).await
        else {
            panic!("a grants file that does not read refuses a granted key");
        };
        assert_eq!(refusal.reason, reason::CORRUPT);
        let own = set_egress(&client, &seat.identity, &echo_only, true).await.unwrap();
        assert_eq!(own.served, 1);
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

type Chunks = futures::channel::mpsc::UnboundedSender<std::io::Result<axum::body::Bytes>>;

async fn lane_upstream() -> (SocketAddr, Chunks) {
    let (chunks, body) = futures::channel::mpsc::unbounded();
    let body = Arc::new(StdMutex::new(Some(body)));
    let streamed = move || {
        let body = body.lock().unwrap().take().unwrap();
        async move {
            let kind = [(axum::http::header::CONTENT_TYPE, "text/event-stream")];
            (kind, axum::body::Body::from_stream(body))
        }
    };
    let listener = ::tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let at = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route("/ping", axum::routing::get(|| async { PONG }))
        .route("/stream", axum::routing::get(streamed))
        .route(
            "/big",
            axum::routing::get(|| async { vec![b'x'; node::BLOCK_BYTES] }),
        );
    ::tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (at, chunks)
}

async fn lane_put(client: &Client, op: egress::Op) -> Outcome {
    let anchor = client.status().await.unwrap().anchor();
    let putter = ed25519::PrivateKey::from_seed(PUTTER_SEED);
    let op = abi::encode(&op);
    let frame = Frame::sign(&putter, NETWORK.as_bytes(), anchor, None, "egress", op);
    client.land(&frame).await.unwrap().1.outcome
}

async fn lane_landed(
    client: &Client,
    changes: &mut (impl futures::Stream<Item = noded::Result<Change>> + Unpin),
    key: &[u8],
) -> Vec<u8> {
    loop {
        if let Some(bytes) = client.get(Layer::Confirmed, "egress", key).await.unwrap() {
            return bytes;
        }
        changes.next().await.unwrap().unwrap();
    }
}

fn box_lanes(egress: bool) -> halo::Genesis {
    let mut lanes = vec![halo::Founded {
        name: blob::contract::LANE.to_owned(),
        cap: halo::Class::Critical,
        pinned: true,
    }];
    if egress {
        lanes.push(halo::Founded {
            name: noded::EGRESS_LANE.to_owned(),
            cap: halo::Class::Member,
            pinned: false,
        });
    }
    halo::Genesis { lanes }
}

fn box_network(root: &Path, egress: bool) -> (Vec<Seat>, Vec<Live>) {
    let seats: Vec<Seat> = (0..2).map(|i| Seat::new(root, &format!("n{i}"))).collect();
    let founding = founding(root, &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    std::fs::write(root.join("halo.params"), abi::encode(&box_lanes(egress))).unwrap();
    for seat in &seats {
        seat.init(&founding);
    }
    let live = seats.iter().map(Seat::start).collect();
    (seats, live)
}

struct Unreached(::tokio::sync::watch::Sender<usize>);

struct Named(bool);

impl tracing::field::Visit for Named {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "event" && value == "egress_relay_unreached" {
            self.0 = true;
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Unreached {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut named = Named(false);
        event.record(&mut named);
        if named.0 {
            self.0.send_modify(|count| *count += 1);
        }
    }
}

struct Boxed {
    key: Vec<u8>,
    die: oneshot::Sender<()>,
    gone: oneshot::Receiver<()>,
    thread: JoinHandle<()>,
}

fn boxed(
    root: &Path,
    client: &Client,
    echo: SocketAddr,
    unreached: ::tokio::sync::watch::Sender<usize>,
) -> Boxed {
    let boxed = Workspace::at(root.join("box"));
    let udp = UdpSocket::bind((outbound_ip(), 0)).unwrap();
    let registered = udp.local_addr().unwrap().to_string();
    runner(&boxed.runtime_dir())
        .start(|context| noded::enroll(context, &boxed, client.clone(), registered, None))
        .unwrap();
    let key = boxed.identity().unwrap().public_key().as_ref().to_vec();
    let services = format!("[services.echo]\nbase = \"http://{echo}\"\n");
    std::fs::write(boxed.dir().join("egress.toml"), services).unwrap();
    let (die, dying) = oneshot::channel::<()>();
    let (alive, gone) = oneshot::channel::<()>();
    let client = client.clone();
    let thread = std::thread::spawn(move || {
        let _alive = alive;
        let counted = tracing_subscriber::registry().with(Unreached(unreached));
        runner(&boxed.runtime_dir()).start(|context| async move {
            let serving = noded::serve_box(&context, boxed, &client, udp).with_subscriber(counted);
            ::tokio::select! {
                served = serving => panic!("the box stopped serving: {served:?}"),
                _ = dying => {}
            }
        })
    });
    Boxed {
        key,
        die,
        gone,
        thread,
    }
}

async fn racing<T>(gone: &mut oneshot::Receiver<()>, waited: impl Future<Output = T>) -> T {
    ::tokio::select! {
        done = waited => done,
        _ = gone => panic!("the box ended while the test waited on it"),
    }
}

#[test]
fn a_box_with_no_node_serves_egress_through_a_relay_and_its_stream_ends_unserved_when_it_dies() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let (seats, live) = box_network(root.path(), true);
    let client = live[0].client.clone();
    let runtime = client_runtime();
    let (echo, chunks) = runtime.block_on(lane_upstream());
    let (unreached, _) = ::tokio::sync::watch::channel(0);
    let Boxed {
        key,
        die,
        mut gone,
        thread,
    } = boxed(root.path(), &client, echo, unreached);

    let id = runtime.block_on(racing(&mut gone, async {
        let mut changes = client.changes("egress").await.unwrap();
        recorded(&client, &mut changes, &key, &["echo"]).await;
        pinged(&client, &mut changes, "echo").await;

        let streamed = egress::Op::Request {
            request: egress::Request::Http {
                service: "echo".to_owned(),
                head: egress::http::Head {
                    method: egress::http::Method::Get,
                    target: "/stream".to_owned(),
                    headers: Vec::new(),
                },
                body: Vec::new(),
            },
            delivery: egress::Delivery::Events {
                deliver: Vec::new(),
            },
            wait: None,
        };
        let Outcome::Applied { output } = lane_put(&client, streamed).await else {
            panic!("the streamed request was not taken");
        };
        let id: u64 = abi::decode(&output).unwrap();
        let first = axum::body::Bytes::from_static(b"data: 1\n\n");
        chunks.unbounded_send(Ok(first)).unwrap();
        let event = lane_landed(&client, &mut changes, &fixture_egress::event_key(id, 0)).await;
        assert_eq!(abi::decode::<Vec<u8>>(&event).unwrap(), b"data: 1\n");
        die.send(()).unwrap();
        id
    }));
    thread.join().unwrap();

    runtime.block_on(async {
        let mut changes = client.changes("egress").await.unwrap();
        let row = client
            .get(Layer::Confirmed, "egress", &fixture_egress::request_key(id))
            .await
            .unwrap()
            .expect("the request the box left mid-stream is still open");
        let row: egress::RequestRow = abi::decode(&row).unwrap();
        reach(&client, row.expiry).await;
        let expired = lane_put(&client, egress::Op::Expire { id }).await;
        assert!(matches!(expired, Outcome::Applied { .. }), "{expired:?}");
        assert_eq!(
            answered(&client, &mut changes, id).await,
            egress::Answer::Failed("expired".to_owned())
        );
        let end = fixture_egress::end_key(id);
        let ended = client.get(Layer::Confirmed, "egress", &end).await.unwrap();
        assert_eq!(ended, None, "the dead box ended the stream");
        for (seat, node) in seats.iter().zip(&live) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

#[test]
fn a_box_whose_lane_is_unlisted_reads_again_only_when_halo_changes_and_serves_once_the_vote_opens_it()
 {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let (seats, live) = box_network(root.path(), false);
    let client = live[0].client.clone();
    let runtime = client_runtime();
    let (echo, _chunks) = runtime.block_on(lane_upstream());
    let (unreached, mut counted) = ::tokio::sync::watch::channel(0);
    let Boxed {
        key,
        die,
        mut gone,
        thread,
    } = boxed(root.path(), &client, echo, unreached);
    runtime.block_on(racing(&mut gone, async {
        let first = *counted.wait_for(|count| *count > 0).await.unwrap();
        let from = client.status().await.unwrap().height;
        reach(&client, from + 2).await;
        assert_eq!(
            *counted.borrow(),
            first,
            "the box tried its relays again with nothing changed on the chain"
        );
        let mut changes = client.changes("egress").await.unwrap();
        let open = halo::Op::Open {
            name: noded::EGRESS_LANE.into(),
            cap: halo::Class::Member,
        };
        let motion = Motion::Role {
            role: role::HALO.into(),
            op: abi::encode(&open),
        };
        membership(&client, &seats[0].identity, Op::Vote(motion)).await;
        recorded(&client, &mut changes, &key, &["echo"]).await;
        pinged(&client, &mut changes, "echo").await;
        die.send(()).unwrap();
    }));
    thread.join().unwrap();
    shut_down(&runtime, &seats.iter().collect::<Vec<_>>(), live);
}

#[test]
fn a_box_sends_an_answer_larger_than_a_block_carries_as_a_failure() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let (seats, live) = box_network(root.path(), true);
    let client = live[0].client.clone();
    let runtime = client_runtime();
    let (echo, _chunks) = runtime.block_on(lane_upstream());
    let (unreached, _) = ::tokio::sync::watch::channel(0);
    let Boxed {
        key,
        die,
        mut gone,
        thread,
    } = boxed(root.path(), &client, echo, unreached);
    runtime.block_on(racing(&mut gone, async {
        let mut changes = client.changes("egress").await.unwrap();
        recorded(&client, &mut changes, &key, &["echo"]).await;
        let big = egress::Op::Request {
            request: egress::Request::Http {
                service: "echo".to_owned(),
                head: egress::http::Head {
                    method: egress::http::Method::Get,
                    target: "/big".to_owned(),
                    headers: Vec::new(),
                },
                body: Vec::new(),
            },
            delivery: egress::Delivery::Whole,
            wait: None,
        };
        let Outcome::Applied { output } = lane_put(&client, big).await else {
            panic!("the request for /big was not taken");
        };
        let id: u64 = abi::decode(&output).unwrap();
        let open = client
            .get(Layer::Confirmed, "egress", &fixture_egress::request_key(id))
            .await
            .unwrap();
        let answer = match open {
            None => answered(&client, &mut changes, id).await,
            Some(row) => {
                let row: egress::RequestRow = abi::decode(&row).unwrap();
                ::tokio::select! {
                    answer = answered(&client, &mut changes, id) => answer,
                    () = reach(&client, row.expiry) => {
                        panic!("no answer landed by the request's expiry at {}", row.expiry)
                    }
                }
            }
        };
        assert_eq!(
            answer,
            egress::Answer::Failed("the answer is larger than a block carries".to_owned())
        );
        die.send(()).unwrap();
    }));
    thread.join().unwrap();
    shut_down(&runtime, &seats.iter().collect::<Vec<_>>(), live);
}

#[test]
fn a_box_answers_a_request_whose_body_no_member_holds_failed_and_serves_the_rest() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let (seats, live) = box_network(root.path(), true);
    let client = live[0].client.clone();
    let runtime = client_runtime();
    let (echo, _chunks) = runtime.block_on(lane_upstream());
    let (unreached, _) = ::tokio::sync::watch::channel(0);
    let Boxed {
        key,
        die,
        mut gone,
        thread,
    } = boxed(root.path(), &client, echo, unreached);
    runtime.block_on(racing(&mut gone, async {
        let mut changes = client.changes("egress").await.unwrap();
        recorded(&client, &mut changes, &key, &["echo"]).await;
        let part = abi::BlobId::Sha256([7; 32]);
        let unheld = egress::Op::Request {
            request: egress::Request::Http {
                service: "echo".to_owned(),
                head: egress::http::Head {
                    method: egress::http::Method::Get,
                    target: "/ping".to_owned(),
                    headers: Vec::new(),
                },
                body: vec![part],
            },
            delivery: egress::Delivery::Whole,
            wait: None,
        };
        let Outcome::Applied { output } = lane_put(&client, unheld).await else {
            panic!("the request whose body no member holds was not taken");
        };
        let id: u64 = abi::decode(&output).unwrap();
        let row = client
            .get(Layer::Confirmed, "egress", &fixture_egress::request_key(id))
            .await
            .unwrap()
            .expect("the request whose body no member holds is open");
        let row: egress::RequestRow = abi::decode(&row).unwrap();
        let mut pings = client.changes("egress").await.unwrap();
        ::tokio::select! {
            () = pinged(&client, &mut pings, "echo") => {}
            () = reach(&client, row.expiry) => panic!(
                "no ping was answered by {}, the expiry of the request whose body no member holds",
                row.expiry
            ),
        }
        let unsent = format!(
            "the relay node does not hold part {} yet; no member gave it",
            abi::hex(part.digest())
        );
        assert_eq!(
            answered(&client, &mut changes, id).await,
            egress::Answer::Failed(unsent)
        );
        die.send(()).unwrap();
    }));
    thread.join().unwrap();
    shut_down(&runtime, &seats.iter().collect::<Vec<_>>(), live);
}
