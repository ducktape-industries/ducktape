use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use abi::role::egress::http::{Head, Method, Response};
use abi::role::egress::{HTTP, StreamEnd};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::header;
use axum::routing::get;
use futures::StreamExt as _;
use futures::channel::mpsc;
use tokio::net::TcpListener;

use super::*;
use crate::egress::call::{self, Blocks, Caller, Clients, Crosspond, Unretried};
use crate::egress::config::{Config, Peer, Reach, Upstream};
use crate::egress::station::Serving;

struct Unrelayed;

impl Crosspond for Unrelayed {
    async fn relay(
        &mut self,
        name: &str,
        _peer: &Peer,
        _link: &abi::NetworkId,
        _blocks: &mut (impl Blocks + Send),
    ) -> Answer {
        panic!("a stream test relayed to {name}")
    }
}

type Chunks = mpsc::UnboundedSender<std::io::Result<Bytes>>;

async fn service() -> (String, Chunks) {
    let (chunks, body) = mpsc::unbounded();
    let body = Arc::new(Mutex::new(Some(body)));
    let streamed = move || {
        let body = body.lock().unwrap().take().unwrap();
        async move {
            let kind = [(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")];
            (kind, Body::from_stream(body))
        }
    };
    let app = Router::new()
        .route("/stream", get(streamed))
        .route("/plain", get(|| async { "event: no\n\n" }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), chunks)
}

fn send(chunks: &Chunks, chunk: &'static str) {
    chunks.unbounded_send(Ok(Bytes::from(chunk))).unwrap();
}

fn events() -> Delivery {
    Delivery::Events { deliver: vec![4] }
}

fn get_row(id: u64, base: &str, path: &str, delivery: Delivery) -> RequestRow {
    RequestRow {
        request: Request::Http {
            service: HTTP.into(),
            head: Head {
                method: Method::Get,
                target: format!("{base}{path}"),
                headers: Vec::new(),
            },
            body: Vec::new(),
        },
        delivery,
        ..row(id)
    }
}

fn assigned(row: &RequestRow) -> Reading {
    Reading {
        assigned: vec![row.clone()],
        serving: vec!["relay".into()],
        member: true,
    }
}

fn call(row: &RequestRow) -> mpsc::UnboundedReceiver<Called> {
    let (called, calls) = mpsc::unbounded();
    let row = row.clone();
    tokio::spawn(async move {
        let config = Config {
            relay: true,
            http: true,
            reach: Reach::Private,
            services: BTreeMap::new(),
            crosspond: BTreeMap::new(),
        };
        let clients = Clients::new(config.reach).unwrap();
        let serving = Serving { config, clients };
        let caller = Caller {
            chain: "dognet-b5b6ea90",
            program: "harness",
            id: row.id,
        };
        let event = |data| called.unbounded_send(Called::Event(data)).unwrap();
        let (request, delivery) = (&row.request, &row.delivery);
        let mut blocks = Unretried;
        let ended = call::answer(
            &serving,
            &caller,
            request,
            delivery,
            Vec::new(),
            &mut blocks,
            &mut Unrelayed,
            event,
        )
        .await;
        called.unbounded_send(ended).unwrap();
    });
    calls
}

fn events_op(id: u64, events: &[(u64, &str)]) -> Op {
    Op::Events(
        events
            .iter()
            .map(|(seq, data)| Event {
                id,
                seq: *seq,
                data: data.as_bytes().to_vec(),
            })
            .collect(),
    )
}

fn event(id: u64, seq: u64, data: &str) -> Op {
    events_op(id, &[(seq, data)])
}

type Asked = Arc<Mutex<Vec<String>>>;

async fn rejoining(marked: bool) -> (String, Asked, Arc<tokio::sync::Notify>) {
    let asked = Asked::default();
    let heard = Arc::new(tokio::sync::Notify::new());
    let (seen, cut) = (asked.clone(), heard.clone());
    let answer = move |headers: axum::http::HeaderMap| {
        let (seen, cut) = (seen.clone(), cut.clone());
        async move {
            let named = |name: &str| {
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            let request = named("ducktape-request").unwrap_or_default();
            let resumed = named("ducktape-heard");
            seen.lock()
                .unwrap()
                .push(format!("{request}:{}", resumed.as_deref().unwrap_or("-")));
            let body = match resumed.and_then(|heard| heard.parse::<usize>().ok()) {
                None => futures::stream::once(async {
                    Ok::<_, std::io::Error>(Bytes::from("data: 1\n\ndata: 2\n\ndata: cut"))
                })
                .chain(futures::stream::once(async move {
                    cut.notified().await;
                    Err(std::io::Error::other("the old pod stopped"))
                }))
                .boxed(),
                Some(heard) => {
                    let after: String =
                        ["data: 1\n\n", "data: 2\n\n", "data: 3\n\n"][heard..].concat();
                    futures::stream::once(
                        async move { Ok::<_, std::io::Error>(Bytes::from(after)) },
                    )
                    .boxed()
                }
            };
            let mut response = axum::response::Response::new(Body::from_stream(body));
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
            if marked {
                headers.insert("ducktape-reattach", "request".parse().unwrap());
            }
            response
        }
    };
    let app = Router::new().route("/rejoin", get(answer));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), asked, heard)
}

struct Ahead {
    tip: u64,
    expiry: u64,
}

impl Blocks for Ahead {
    fn tip(&mut self) -> u64 {
        self.tip
    }

    async fn expiry(&mut self) -> Option<u64> {
        Some(self.expiry)
    }

    async fn changed(&mut self) -> bool {
        self.tip += 1;
        true
    }
}

fn service_row(id: u64, service: &str, target: &str) -> RequestRow {
    RequestRow {
        request: Request::Http {
            service: service.into(),
            head: Head {
                method: Method::Get,
                target: target.into(),
                headers: Vec::new(),
            },
            body: Vec::new(),
        },
        delivery: events(),
        ..row(id)
    }
}

fn pods_serving(base: &str) -> Serving {
    let upstream = Upstream {
        base: url::Url::parse(base).unwrap(),
        secrets: Vec::new(),
    };
    let config = Config {
        relay: true,
        http: true,
        reach: Reach::Private,
        services: BTreeMap::from([("pods".to_owned(), upstream)]),
        crosspond: BTreeMap::new(),
    };
    let clients = Clients::new(config.reach).unwrap();
    Serving { config, clients }
}

async fn rejoined(
    row: &RequestRow,
    serving: &Serving,
    blocks: &mut Ahead,
    heard: &tokio::sync::Notify,
    mut event: impl FnMut(Vec<u8>),
) -> Called {
    let caller = Caller {
        chain: "dognet-b5b6ea90",
        program: "harness",
        id: row.id,
    };
    let told = |data| {
        event(data);
        heard.notify_one();
    };
    call::answer(
        serving,
        &caller,
        &row.request,
        &row.delivery,
        Vec::new(),
        blocks,
        &mut Unrelayed,
        told,
    )
    .await
}

#[tokio::test]
async fn a_stream_the_service_marked_reattachable_is_asked_again_and_its_events_number_on() {
    let (base, asked, heard) = rejoining(true).await;
    let serving = pods_serving(&base);
    let row = service_row(5, "pods", "/rejoin");
    let mut desk = served();
    desk.read(assigned(&row));
    let mut blocks = Ahead {
        tip: 10,
        expiry: 74,
    };
    let mut heard_events = Vec::new();
    let ended = rejoined(&row, &serving, &mut blocks, &heard, |data| {
        heard_events.push(data)
    })
    .await;
    for data in heard_events {
        desk.called(5, Called::Event(data));
    }
    desk.called(5, ended.clone());
    assert_eq!(*asked.lock().unwrap(), ["5:-", "5:2"]);
    assert_eq!(blocks.tip, 11);
    assert_eq!(desk.launch(), [Step::Send(flight(5, 0))]);
    let all = [(0, "data: 1\n"), (1, "data: 2\n"), (2, "data: 3\n")];
    assert_eq!(desk.op(&flight(5, 0)), Some(events_op(5, &all)));
    let next = desk.sent(&flight(5, 0), Sent::Pending(b"frame 0".to_vec()));
    assert_eq!(next, Some(Step::Send(flight(5, 1))));
    let Some(Op::EndStream { events, end, .. }) = desk.op(&flight(5, 1)) else {
        panic!("no end after the events: {ended:?}");
    };
    assert_eq!(events, 3);
    let StreamEnd::Complete { status, headers } = end else {
        panic!("the stream did not complete: {end:?}");
    };
    assert_eq!(status, 200);
    let mark = ("ducktape-reattach".to_owned(), "request".to_owned());
    assert!(headers.contains(&mark), "{headers:?}");
}

#[tokio::test]
async fn a_stream_is_not_asked_again_unmarked_over_http_or_when_its_expiry_comes_first() {
    let stream_broken = Called::End(StreamEnd::Cut {
        reason: "stream_broken".into(),
    });
    let (unmarked, unmarked_asked, unmarked_heard) = rejoining(false).await;
    let (marked, http_asked, http_heard) = rejoining(true).await;
    let (late, late_asked, late_heard) = rejoining(true).await;
    let cases = [
        (
            pods_serving(&unmarked),
            service_row(6, "pods", "/rejoin"),
            unmarked_asked,
            unmarked_heard,
            74,
        ),
        (
            pods_serving(&marked),
            service_row(7, HTTP, &format!("{marked}/rejoin")),
            http_asked,
            http_heard,
            74,
        ),
        (
            pods_serving(&late),
            service_row(8, "pods", "/rejoin"),
            late_asked,
            late_heard,
            12,
        ),
    ];
    for (serving, row, asked, heard, expiry) in cases {
        let mut blocks = Ahead { tip: 10, expiry };
        let mut heard_events = Vec::new();
        let ended = rejoined(&row, &serving, &mut blocks, &heard, |data| {
            heard_events.push(data)
        })
        .await;
        assert_eq!(ended, stream_broken, "request {}", row.id);
        let read = [b"data: 1\n".to_vec(), b"data: 2\n".to_vec()];
        assert_eq!(heard_events, read, "request {}", row.id);
        assert_eq!(asked.lock().unwrap().len(), 1, "request {}", row.id);
        assert_eq!(blocks.tip, 10, "request {}", row.id);
    }
}

fn whole(called: Called) -> Response {
    let Called::Whole(Answer::Output(bytes)) = called else {
        panic!("not a whole response: {called:?}");
    };
    abi::decode(&bytes).unwrap()
}

#[tokio::test]
async fn a_stream_sends_each_event_as_it_arrives_while_earlier_frames_wait_for_a_block() {
    let (base, chunks) = service().await;
    let row = get_row(1, &base, "/stream", events());
    let mut desk = served();
    assert_eq!(desk.read(assigned(&row)), [Step::Call(row.clone())]);
    let mut calls = call(&row);

    send(&chunks, "event: delta\ndata: 1\n\n");
    let first = calls.next().await.unwrap();
    assert_eq!(first, Called::Event(b"event: delta\ndata: 1\n".to_vec()));
    desk.called(1, first);
    assert_eq!(desk.launch(), [Step::Send(flight(1, 0))]);
    assert_eq!(
        desk.op(&flight(1, 0)),
        Some(event(1, 0, "event: delta\ndata: 1\n"))
    );
    assert_eq!(
        desk.sent(&flight(1, 0), Sent::Pending(b"frame 0".to_vec())),
        None
    );

    send(&chunks, "data: 2");
    send(&chunks, "\n\n");
    let second = calls.next().await.unwrap();
    desk.called(1, second);
    assert_eq!(desk.launch(), [Step::Send(flight(1, 1))]);
    assert_eq!(desk.op(&flight(1, 1)), Some(event(1, 1, "data: 2\n")));
    assert_eq!(
        desk.sent(&flight(1, 1), Sent::Pending(b"frame 1".to_vec())),
        None
    );
    let both = [Step::Send(flight(1, 0)), Step::Send(flight(1, 1))];
    assert_eq!(desk.read(assigned(&row)), both);
    assert_eq!(desk.frame(&flight(1, 0)), Some(&b"frame 0"[..]));
    assert_eq!(desk.sent(&flight(1, 0), Sent::Carried), None);
    assert_eq!(
        desk.sent(&flight(1, 1), Sent::Pending(b"frame 1".to_vec())),
        None
    );

    drop(chunks);
    let end = calls.next().await.unwrap();
    let Called::End(StreamEnd::Complete { status, headers }) = &end else {
        panic!("the stream did not complete: {end:?}");
    };
    assert_eq!(*status, 200);
    let kind = (
        "content-type".into(),
        "text/event-stream; charset=utf-8".into(),
    );
    assert!(headers.contains(&kind), "{headers:?}");
    let complete = StreamEnd::Complete {
        status: *status,
        headers: headers.clone(),
    };
    desk.called(1, end);
    assert_eq!(desk.launch(), [Step::Send(flight(1, 2))]);
    let closing = Op::EndStream {
        id: 1,
        events: 2,
        end: complete,
    };
    assert_eq!(desk.op(&flight(1, 2)), Some(closing));
    desk.sent(&flight(1, 2), Sent::Pending(b"frame 2".to_vec()));
    let last = [Step::Send(flight(1, 1)), Step::Send(flight(1, 2))];
    assert_eq!(desk.read(assigned(&row)), last);
    assert_eq!(desk.sent(&flight(1, 1), Sent::Carried), None);
    assert_eq!(desk.sent(&flight(1, 2), Sent::Carried), None);
    assert!(desk.read(assigned(&row)).is_empty());
    assert_eq!(calls.next().await, None);
}

#[tokio::test]
async fn a_stream_the_service_cuts_ends_cut_after_the_events_already_read() {
    let (base, chunks) = service().await;
    let row = get_row(2, &base, "/stream", events());
    let mut desk = served();
    desk.read(assigned(&row));
    let mut calls = call(&row);

    send(&chunks, "data: only\n\ndata: unfinished");
    let only = calls.next().await.unwrap();
    desk.called(2, only);
    assert_eq!(desk.launch(), [Step::Send(flight(2, 0))]);
    chunks
        .unbounded_send(Err(std::io::Error::other("the service fell over")))
        .unwrap();
    let cut = calls.next().await.unwrap();
    let stream_broken = StreamEnd::Cut {
        reason: "stream_broken".into(),
    };
    assert_eq!(cut, Called::End(stream_broken.clone()));
    desk.called(2, cut);
    assert!(desk.launch().is_empty());
    assert_eq!(desk.op(&flight(2, 0)), Some(event(2, 0, "data: only\n")));
    assert_eq!(
        desk.sent(&flight(2, 0), Sent::Pending(b"frame 0".to_vec())),
        Some(Step::Send(flight(2, 1)))
    );
    let closing = Op::EndStream {
        id: 2,
        events: 1,
        end: stream_broken,
    };
    assert_eq!(desk.op(&flight(2, 1)), Some(closing));
}

#[tokio::test]
async fn a_request_for_events_answered_without_a_stream_comes_back_whole() {
    let (base, _chunks) = service().await;
    let row = get_row(3, &base, "/plain", events());
    let mut calls = call(&row);
    let response = whole(calls.next().await.unwrap());
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"event: no\n\n");
    assert_eq!(calls.next().await, None);
}

#[tokio::test]
async fn a_whole_request_to_a_streaming_service_comes_back_whole() {
    let (base, chunks) = service().await;
    let row = get_row(4, &base, "/stream", Delivery::Whole);
    send(&chunks, "data: a\n\n");
    send(&chunks, "data: b\n\n");
    drop(chunks);
    let mut calls = call(&row);
    let response = whole(calls.next().await.unwrap());
    assert_eq!(response.body, b"data: a\n\ndata: b\n\n");
    assert_eq!(calls.next().await, None);
}

#[test]
fn an_event_too_large_for_a_block_cuts_the_stream_and_later_events_are_dropped() {
    let mut desk = served();
    desk.read(reading(&[5], &["relay"], true));
    desk.called(5, Called::Event(b"data: 0\n".to_vec()));
    desk.called(5, Called::Event(vec![b'x'; 8]));
    desk.called(5, Called::Event(b"data: 2\n".to_vec()));
    assert_eq!(desk.launch(), [Step::Send(flight(5, 0))]);
    let capacity = || Sent::Refused(Refusal::new(reason::CAPACITY, "too large"));
    assert_eq!(
        desk.sent(&flight(5, 0), capacity()),
        Some(Step::Send(flight(5, 0)))
    );
    assert_eq!(desk.op(&flight(5, 0)), Some(event(5, 0, "data: 0\n")));
    assert_eq!(
        desk.sent(&flight(5, 0), Sent::Pending(b"frame 0".to_vec())),
        Some(Step::Send(flight(5, 1)))
    );
    assert_eq!(
        desk.sent(&flight(5, 1), capacity()),
        Some(Step::Send(flight(5, 1)))
    );
    assert_eq!(desk.op(&flight(5, 1)), Some(event(5, 1, "xxxxxxxx")));
    assert_eq!(
        desk.sent(&flight(5, 1), capacity()),
        Some(Step::Send(flight(5, 1)))
    );
    let too_large = StreamEnd::Cut {
        reason: "event_too_large".into(),
    };
    let closing = Op::EndStream {
        id: 5,
        events: 1,
        end: too_large,
    };
    assert_eq!(desk.op(&flight(5, 1)), Some(closing));
    desk.called(5, Called::Event(b"data: 3\n".to_vec()));
    let complete = StreamEnd::Complete {
        status: 200,
        headers: Vec::new(),
    };
    desk.called(5, Called::End(complete));
    assert_eq!(desk.sent(&flight(5, 1), Sent::Carried), None);
    assert!(desk.launch().is_empty());
    assert_eq!(desk.op(&flight(5, 1)), None);
}

#[test]
fn every_event_read_while_the_worker_is_busy_goes_out_together_in_one_frame() {
    let mut desk = served();
    desk.read(reading(&[6], &["relay"], true));
    for n in 0..=2 {
        desk.called(6, Called::Event(format!("data: {n}\n").into_bytes()));
    }
    assert_eq!(desk.launch(), [Step::Send(flight(6, 0))]);
    let together = events_op(6, &[(0, "data: 0\n"), (1, "data: 1\n"), (2, "data: 2\n")]);
    assert_eq!(desk.op(&flight(6, 0)), Some(together));
    desk.sent(&flight(6, 0), Sent::Pending(b"frame 0".to_vec()));
    desk.called(6, Called::Event(b"data: 3\n".to_vec()));
    let complete = StreamEnd::Complete {
        status: 200,
        headers: Vec::new(),
    };
    desk.called(6, Called::End(complete.clone()));
    assert_eq!(desk.launch(), [Step::Send(flight(6, 1))]);
    assert_eq!(desk.op(&flight(6, 1)), Some(event(6, 3, "data: 3\n")));
    assert_eq!(
        desk.sent(&flight(6, 1), Sent::Pending(b"frame 1".to_vec())),
        Some(Step::Send(flight(6, 2)))
    );
    let closing = Op::EndStream {
        id: 6,
        events: 4,
        end: complete,
    };
    assert_eq!(desk.op(&flight(6, 2)), Some(closing));
    assert_eq!(desk.frame(&flight(6, 0)), Some(&b"frame 0"[..]));
}

#[test]
fn a_frame_not_yet_in_the_pool_holds_the_events_read_after_it() {
    let mut desk = served();
    desk.read(reading(&[9], &["relay"], true));
    desk.called(9, Called::Event(b"data: 0\n".to_vec()));
    assert_eq!(desk.launch(), [Step::Send(flight(9, 0))]);
    assert_eq!(desk.sent(&flight(9, 0), Sent::Untaken), None);
    desk.called(9, Called::Event(b"data: 1\n".to_vec()));
    desk.called(9, Called::Event(b"data: 2\n".to_vec()));
    assert!(desk.launch().is_empty());
    assert_eq!(
        desk.read(reading(&[9], &["relay"], true)),
        [Step::Send(flight(9, 0))]
    );
    assert_eq!(
        desk.sent(&flight(9, 0), Sent::Pending(b"frame 0".to_vec())),
        Some(Step::Send(flight(9, 1)))
    );
    let after = events_op(9, &[(1, "data: 1\n"), (2, "data: 2\n")]);
    assert_eq!(desk.op(&flight(9, 1)), Some(after));
}

#[test]
fn a_stale_frame_is_signed_again_at_once_with_its_own_events() {
    let mut desk = served();
    desk.read(reading(&[7], &["relay"], true));
    desk.called(7, Called::Event(b"data: 0\n".to_vec()));
    desk.launch();
    desk.sent(&flight(7, 0), Sent::Pending(b"frame 0".to_vec()));
    desk.called(7, Called::Event(b"data: 1\n".to_vec()));
    assert_eq!(desk.launch(), [Step::Send(flight(7, 1))]);
    desk.sent(&flight(7, 1), Sent::Pending(b"frame 1".to_vec()));
    assert_eq!(
        desk.sent(&flight(7, 0), Sent::Stale),
        Some(Step::Send(flight(7, 0)))
    );
    assert_eq!(desk.frame(&flight(7, 0)), None);
    assert_eq!(desk.op(&flight(7, 0)), Some(event(7, 0, "data: 0\n")));
    assert_eq!(desk.frame(&flight(7, 1)), Some(&b"frame 1"[..]));
}

#[test]
fn events_too_large_for_one_block_together_go_out_in_halves_in_order() {
    let mut desk = served();
    desk.read(reading(&[8], &["relay"], true));
    desk.called(8, Called::Event(b"data: 0\n".to_vec()));
    desk.launch();
    desk.sent(&flight(8, 0), Sent::Pending(b"frame 0".to_vec()));
    for n in 1..=4 {
        desk.called(8, Called::Event(format!("data: {n}\n").into_bytes()));
    }
    assert_eq!(desk.launch(), [Step::Send(flight(8, 1))]);
    let capacity = || Sent::Refused(Refusal::new(reason::CAPACITY, "too large"));
    assert_eq!(
        desk.sent(&flight(8, 1), capacity()),
        Some(Step::Send(flight(8, 1)))
    );
    let front = events_op(8, &[(1, "data: 1\n"), (2, "data: 2\n")]);
    assert_eq!(desk.op(&flight(8, 1)), Some(front));
    assert_eq!(
        desk.sent(&flight(8, 1), Sent::Pending(b"frame 1".to_vec())),
        Some(Step::Send(flight(8, 2)))
    );
    let back = events_op(8, &[(3, "data: 3\n"), (4, "data: 4\n")]);
    assert_eq!(desk.op(&flight(8, 2)), Some(back));
}
