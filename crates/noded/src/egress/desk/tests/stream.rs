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
use crate::egress::call::{self, Caller, Clients};
use crate::egress::config::{Config, Reach};

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
        };
        let clients = Clients::new(config.reach).unwrap();
        let caller = Caller {
            chain: "dognet-b5b6ea90",
            program: "harness",
            id: row.id,
        };
        let event = |data| called.unbounded_send(Called::Event(data)).unwrap();
        let (request, delivery) = (&row.request, &row.delivery);
        let ended = call::answer(
            &clients,
            &config,
            &caller,
            request,
            delivery,
            Vec::new(),
            event,
        )
        .await;
        called.unbounded_send(ended).unwrap();
    });
    calls
}

fn event(id: u64, seq: u64, data: &str) -> Op {
    Op::Event(Event {
        id,
        seq,
        data: data.as_bytes().to_vec(),
    })
}

fn whole(called: Called) -> Response {
    let Called::Whole(Answer::Output(bytes)) = called else {
        panic!("not a whole response: {called:?}");
    };
    abi::decode(&bytes).unwrap()
}

#[tokio::test]
async fn a_stream_goes_out_one_event_frame_at_a_time_in_order_and_ends_with_its_count() {
    let (base, chunks) = service().await;
    let row = get_row(1, &base, "/stream", events());
    let mut desk = served();
    assert_eq!(desk.read(assigned(&row)), [Step::Call(row.clone())]);
    let mut calls = call(&row);
    let purpose = Purpose::Request(1);

    send(&chunks, "event: delta\ndata: 1\n\n");
    let first = calls.next().await.unwrap();
    assert_eq!(first, Called::Event(b"event: delta\ndata: 1\n".to_vec()));
    assert_eq!(desk.called(1, first), Some(Step::Send(purpose.clone())));
    assert_eq!(
        desk.op(&purpose),
        Some(event(1, 0, "event: delta\ndata: 1\n"))
    );
    assert_eq!(
        desk.sent(&purpose, Sent::Pending(b"frame 0".to_vec())),
        None
    );

    send(&chunks, "data: 2");
    send(&chunks, "\n\n");
    let second = calls.next().await.unwrap();
    assert_eq!(desk.called(1, second), None);
    assert_eq!(
        desk.op(&purpose),
        Some(event(1, 0, "event: delta\ndata: 1\n"))
    );
    assert_eq!(desk.read(assigned(&row)), [Step::Send(purpose.clone())]);
    assert_eq!(desk.frame(&purpose), Some(&b"frame 0"[..]));
    assert_eq!(
        desk.sent(&purpose, Sent::Pending(b"frame 0".to_vec())),
        None
    );
    assert_eq!(
        desk.sent(&purpose, Sent::Carried),
        Some(Step::Send(purpose.clone()))
    );
    assert_eq!(desk.op(&purpose), Some(event(1, 1, "data: 2\n")));
    assert_eq!(desk.frame(&purpose), None);
    assert_eq!(
        desk.sent(&purpose, Sent::Pending(b"frame 1".to_vec())),
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
    assert_eq!(desk.called(1, end), None);
    assert_eq!(
        desk.sent(&purpose, Sent::Carried),
        Some(Step::Send(purpose.clone()))
    );
    let closing = Op::EndStream {
        id: 1,
        events: 2,
        end: complete,
    };
    assert_eq!(desk.op(&purpose), Some(closing));
    assert_eq!(desk.sent(&purpose, Sent::Carried), None);
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
    let purpose = Purpose::Request(2);

    send(&chunks, "data: only\n\ndata: unfinished");
    let only = calls.next().await.unwrap();
    assert_eq!(desk.called(2, only), Some(Step::Send(purpose.clone())));
    chunks
        .unbounded_send(Err(std::io::Error::other("the service fell over")))
        .unwrap();
    let cut = calls.next().await.unwrap();
    let stream_broken = StreamEnd::Cut {
        reason: "stream_broken".into(),
    };
    assert_eq!(cut, Called::End(stream_broken.clone()));
    assert_eq!(desk.called(2, cut), None);
    assert_eq!(desk.op(&purpose), Some(event(2, 0, "data: only\n")));
    assert_eq!(
        desk.sent(&purpose, Sent::Carried),
        Some(Step::Send(purpose.clone()))
    );
    let closing = Op::EndStream {
        id: 2,
        events: 1,
        end: stream_broken,
    };
    assert_eq!(desk.op(&purpose), Some(closing));
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
    let purpose = Purpose::Request(5);
    desk.called(5, Called::Event(b"data: 0\n".to_vec()));
    desk.called(5, Called::Event(vec![b'x'; 8]));
    desk.called(5, Called::Event(b"data: 2\n".to_vec()));
    desk.sent(&purpose, Sent::Carried);
    let capacity = Refusal::new(reason::CAPACITY, "too large");
    assert_eq!(
        desk.sent(&purpose, Sent::Refused(capacity)),
        Some(Step::Send(purpose.clone()))
    );
    let too_large = StreamEnd::Cut {
        reason: "event_too_large".into(),
    };
    let closing = Op::EndStream {
        id: 5,
        events: 1,
        end: too_large,
    };
    assert_eq!(desk.op(&purpose), Some(closing));
    assert_eq!(desk.called(5, Called::Event(b"data: 3\n".to_vec())), None);
    let complete = StreamEnd::Complete {
        status: 200,
        headers: Vec::new(),
    };
    assert_eq!(desk.called(5, Called::End(complete)), None);
    assert_eq!(desk.sent(&purpose, Sent::Carried), None);
    assert_eq!(desk.op(&purpose), None);
}
