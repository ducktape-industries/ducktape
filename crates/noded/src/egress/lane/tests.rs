use abi::reason;
use abi::role::egress::http::{Head, Method, Response};
use abi::role::egress::{Delivery, Event, Request, StreamEnd};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::header;
use axum::routing::{get, post};
use commonware_cryptography::Signer as _;
use tokio::io::{AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf, duplex};
use tokio::net::TcpListener;

use super::*;

const NETWORK: &str = "dognet-b5b6ea90";
const ROLE: &str = "egress";
const SERVICE: &str = "echo";

type Chunks = mpsc::UnboundedSender<std::io::Result<Bytes>>;

async fn service() -> (String, Chunks, oneshot::Receiver<()>) {
    let (chunks, body) = mpsc::unbounded();
    let (open, hung_up) = oneshot::channel::<()>();
    let body = Arc::new(Mutex::new(Some((body, open))));
    let streamed = move || {
        let (body, open) = body.lock().unwrap().take().unwrap();
        async move {
            let kind = [(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")];
            let body = body.map(move |chunk| {
                let _open = &open;
                chunk
            });
            (kind, Body::from_stream(body))
        }
    };
    let app = Router::new().route("/stream", get(streamed)).route(
        "/whole",
        post(|body: String| async move { format!("heard {body}") }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), chunks, hung_up)
}

fn send(chunks: &Chunks, chunk: &'static str) {
    chunks.unbounded_send(Ok(Bytes::from(chunk))).unwrap();
}

fn key() -> PrivateKey {
    PrivateKey::from_seed(7)
}

fn server(base: &str) -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::at(dir.path());
    let text = format!("[services.{SERVICE}]\nbase = \"{base}\"\n");
    std::fs::write(workspace.dir().join("egress.toml"), text).unwrap();
    let station = Station::open(workspace).unwrap();
    (dir, Server::new(key(), station))
}

fn tip(height: u64) -> Tip {
    Tip {
        height,
        id: [height as u8; 32],
    }
}

fn row(id: u64, method: Method, path: &str, delivery: Delivery) -> RequestRow {
    RequestRow {
        id,
        requester: "harness".into(),
        request: Request::Http {
            service: SERVICE.into(),
            head: Head {
                method,
                target: path.into(),
                headers: Vec::new(),
            },
            body: Vec::new(),
        },
        delivery,
        node: key().public_key().as_ref().to_vec(),
        made: 1,
        expiry: 0,
    }
}

fn whole(id: u64) -> RequestRow {
    row(id, Method::Post, "/whole", Delivery::Whole)
}

fn output(op: Op) -> (u64, Response) {
    let Op::Answer {
        id,
        answer: Answer::Output(bytes),
    } = op
    else {
        panic!("not an answer with output: {op:?}");
    };
    (id, abi::decode(&bytes).unwrap())
}

fn heard(id: u64, body: &str, op: Op) {
    let (answered, response) = output(op);
    assert_eq!(answered, id);
    assert_eq!(response.status, 200);
    assert_eq!(response.body, format!("heard {body}").into_bytes());
}

fn events(id: u64, seq: u64, data: &str) -> Op {
    Op::Events(vec![Event {
        id,
        seq,
        data: data.as_bytes().to_vec(),
    }])
}

struct Relay {
    reader: ReadHalf<DuplexStream>,
    writer: WriteHalf<DuplexStream>,
}

impl Relay {
    fn new() -> (Relay, DuplexStream) {
        let (near, far) = duplex(BLOCK_BYTES);
        let (reader, writer) = tokio::io::split(near);
        (Relay { reader, writer }, far)
    }

    async fn tell(&mut self, message: ToBox) {
        write(&mut self.writer, &message).await.unwrap();
    }

    async fn hello(&mut self) {
        let network = NETWORK.to_owned();
        self.tell(ToBox::Hello { network }).await;
    }

    async fn body(&mut self, id: u64, body: &str) {
        let body = Ok(body.as_bytes().to_vec());
        self.tell(ToBox::Body { id, body }).await;
    }

    async fn reading(&mut self, height: u64, assigned: Vec<RequestRow>) {
        let message = ToBox::Read {
            tip: tip(height),
            role: ROLE.into(),
            assigned,
            serving: vec![SERVICE.to_owned()],
            member: true,
        };
        self.tell(message).await;
    }

    async fn frame(&mut self) -> (u64, Frame) {
        let ToNode::Frame { tag, frame } = read(&mut self.reader, BLOCK_BYTES).await.unwrap();
        let frame = Frame::decode(&frame).unwrap();
        let submission = frame.verify(NETWORK.as_bytes()).unwrap();
        assert_eq!(submission.signer, key().public_key().as_ref());
        assert_eq!(submission.target, ROLE);
        (tag, frame)
    }

    async fn op(&mut self, verdict: Verdict) -> (Tip, Op) {
        let (tag, frame) = self.frame().await;
        self.tell(ToBox::Taken { tag, verdict }).await;
        let op = abi::decode(&frame.body.payload).unwrap();
        (frame.body.anchor, op)
    }
}

#[tokio::test]
async fn a_box_answers_its_assigned_request_whole_with_a_frame_it_signs_at_the_relays_tip() {
    let (base, _chunks, _) = service().await;
    let (_dir, mut server) = server(&base);
    let (mut relay, stream) = Relay::new();
    let relaying = async move {
        relay.hello().await;
        relay.body(1, "ping").await;
        relay.reading(5, vec![whole(1)]).await;
        let (anchor, op) = relay.op(Verdict::Pending).await;
        assert_eq!(anchor, tip(5));
        heard(1, "ping", op);
    };
    let (served, ()) = tokio::join!(server.session(stream), relaying);
    assert!(served);
}

#[tokio::test]
async fn a_box_streams_each_event_once_the_frame_before_it_is_pooled_and_then_ends_the_stream() {
    let (base, chunks, _) = service().await;
    let (_dir, mut server) = server(&base);
    let (mut relay, stream) = Relay::new();
    let deliver = Delivery::Events { deliver: vec![4] };
    let streamed = row(1, Method::Get, "/stream", deliver);
    let relaying = async move {
        relay.hello().await;
        relay.body(1, "").await;
        relay.reading(5, vec![streamed]).await;
        send(&chunks, "data: 1\n\n");
        assert_eq!(
            relay.op(Verdict::Pending).await.1,
            events(1, 0, "data: 1\n")
        );
        send(&chunks, "data: 2");
        send(&chunks, "\n\n");
        assert_eq!(
            relay.op(Verdict::Pending).await.1,
            events(1, 1, "data: 2\n")
        );
        drop(chunks);
        let (_, end) = relay.op(Verdict::Pending).await;
        let Op::EndStream {
            id: 1,
            events: 2,
            end: StreamEnd::Complete { status: 200, .. },
        } = end
        else {
            panic!("the stream did not end complete after two events: {end:?}");
        };
    };
    let (served, ()) = tokio::join!(server.session(stream), relaying);
    assert!(served);
}

#[tokio::test]
async fn a_box_that_loses_its_relay_signs_the_answer_again_for_the_next_and_takes_carried_as_done()
{
    let (base, _chunks, _) = service().await;
    let (_dir, mut server) = server(&base);
    let (silent, stream) = Relay::new();
    drop(silent);
    assert!(!server.session(stream).await);

    let (mut first, stream) = Relay::new();
    let dropped = async move {
        first.hello().await;
        first.body(1, "one").await;
        first.reading(5, vec![whole(1)]).await;
        let (_, frame) = first.frame().await;
        frame.body.anchor
    };
    let (served, anchor) = tokio::join!(server.session(stream), dropped);
    assert!(served);
    assert_eq!(anchor, tip(5));

    let (mut second, stream) = Relay::new();
    let carried = async move {
        second.hello().await;
        second.reading(9, vec![whole(1)]).await;
        let (anchor, op) = second.op(Verdict::Carried).await;
        assert_eq!(anchor, tip(9));
        heard(1, "one", op);
        second.body(2, "two").await;
        second.reading(10, vec![whole(1), whole(2)]).await;
        let (anchor, op) = second.op(Verdict::Pending).await;
        assert_eq!(anchor, tip(10));
        heard(2, "two", op);
    };
    let (served, ()) = tokio::join!(server.session(stream), carried);
    assert!(served);
}

#[tokio::test]
async fn each_message_reads_back_as_written_and_a_short_or_oversized_one_is_refused() {
    let to_box = [
        ToBox::Hello {
            network: NETWORK.to_owned(),
        },
        ToBox::Body {
            id: 1,
            body: Ok(b"body".to_vec()),
        },
        ToBox::Body {
            id: 2,
            body: Err("part 00 is not on this node".to_owned()),
        },
        ToBox::Read {
            tip: tip(3),
            role: ROLE.into(),
            assigned: vec![whole(1)],
            serving: vec![SERVICE.to_owned()],
            member: true,
        },
        ToBox::Unbound,
        ToBox::Taken {
            tag: 4,
            verdict: Verdict::Refused(Refusal::new(reason::CAPACITY, "too large")),
        },
        ToBox::Taken {
            tag: 5,
            verdict: Verdict::Carried,
        },
    ];
    let (mut near, mut far) = duplex(BLOCK_BYTES);
    for message in &to_box {
        write(&mut near, message).await.unwrap();
    }
    for message in to_box {
        assert_eq!(
            read::<_, ToBox>(&mut far, BLOCK_BYTES).await.unwrap(),
            message
        );
    }

    let frame = ToNode::Frame {
        tag: 6,
        frame: vec![1, 2, 3],
    };
    let size = borsh::to_vec(&frame).unwrap().len();
    write(&mut near, &frame).await.unwrap();
    assert_eq!(read::<_, ToNode>(&mut far, size).await.unwrap(), frame);
    write(&mut near, &frame).await.unwrap();
    let oversized = read::<_, ToNode>(&mut far, size - 1).await.unwrap_err();
    assert_eq!(oversized.kind(), std::io::ErrorKind::InvalidData);

    let (mut near, mut far) = duplex(BLOCK_BYTES);
    near.write_all(&(size as u32).to_le_bytes()).await.unwrap();
    near.write_all(&[0; 3]).await.unwrap();
    drop(near);
    let short = read::<_, ToNode>(&mut far, BLOCK_BYTES).await.unwrap_err();
    assert_eq!(short.kind(), std::io::ErrorKind::UnexpectedEof);
}

fn streamed(id: u64) -> RequestRow {
    let deliver = Delivery::Events { deliver: vec![4] };
    row(id, Method::Get, "/stream", deliver)
}

async fn streaming(relay: &mut Relay, chunks: &Chunks) {
    relay.hello().await;
    relay.body(1, "").await;
    relay.reading(5, vec![streamed(1)]).await;
    send(chunks, "data: 1\n\n");
    assert_eq!(
        relay.op(Verdict::Pending).await.1,
        events(1, 0, "data: 1\n")
    );
}

#[tokio::test]
async fn a_box_ends_a_call_whose_request_leaves_the_assigned_list() {
    let (base, chunks, hung_up) = service().await;
    let (_dir, mut server) = server(&base);
    let (mut relay, stream) = Relay::new();
    let relaying = async move {
        streaming(&mut relay, &chunks).await;
        relay.reading(6, Vec::new()).await;
        assert!(hung_up.await.is_err(), "the upstream heard the call end");
    };
    let (served, ()) = tokio::join!(server.session(stream), relaying);
    assert!(served);
}

#[tokio::test]
async fn a_box_ends_every_call_when_the_relay_says_the_network_binds_no_egress_role() {
    let (base, chunks, hung_up) = service().await;
    let (_dir, mut server) = server(&base);
    let (mut relay, stream) = Relay::new();
    let relaying = async move {
        streaming(&mut relay, &chunks).await;
        relay.tell(ToBox::Unbound).await;
        assert!(hung_up.await.is_err(), "the upstream heard the call end");
    };
    let (served, ()) = tokio::join!(server.session(stream), relaying);
    assert!(served);
}

#[tokio::test]
async fn a_box_keeps_its_calls_past_a_lost_relay_and_ends_them_when_it_is_dropped() {
    let (base, chunks, hung_up) = service().await;
    let (_dir, mut server) = server(&base);
    let (mut relay, stream) = Relay::new();
    let relaying = async move {
        streaming(&mut relay, &chunks).await;
        chunks
    };
    let (served, chunks) = tokio::join!(server.session(stream), relaying);
    assert!(served);
    drop(server);
    assert!(hung_up.await.is_err(), "the upstream heard the call end");
    drop(chunks);
}

#[tokio::test]
async fn a_box_holds_no_call_for_a_request_that_left_the_assigned_list() {
    let (base, _chunks, _) = service().await;
    let (_dir, mut server) = server(&base);
    let (mut relay, stream) = Relay::new();
    let relaying = async move {
        relay.hello().await;
        for id in 1..=3 {
            relay.body(id, "ping").await;
        }
        relay.reading(5, (1..=3).map(whole).collect()).await;
        let mut answered = Vec::new();
        for _ in 1..=3 {
            let (id, _) = output(relay.op(Verdict::Pending).await.1);
            answered.push(id);
        }
        answered.sort();
        assert_eq!(answered, [1, 2, 3]);
        relay.body(4, "four").await;
        relay.reading(6, vec![whole(4)]).await;
        heard(4, "four", relay.op(Verdict::Pending).await.1);
    };
    let (served, ()) = tokio::join!(server.session(stream), relaying);
    assert!(served);
    assert_eq!(server.calls.running(), [4]);
}
