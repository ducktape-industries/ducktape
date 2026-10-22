use std::collections::BTreeMap;
use std::net::TcpListener as Bound;

use abi::role::egress::RELAY;
use abi::role::egress::http::Method;
use axum::Router;
use axum::http::{HeaderMap, HeaderName, Method as Verb, Uri};
use axum::response::Redirect;
use axum::routing::any;
use tokio::net::TcpListener;

use super::*;

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
}

async fn echo(verb: Verb, uri: Uri, headers: HeaderMap, body: String) -> (HeaderMap, String) {
    let seen = [
        "x-api-key",
        "ducktape-chain",
        "ducktape-program",
        "ducktape-request",
    ]
    .map(|name| header(&headers, name))
    .join(" ");
    let mut answered = HeaderMap::new();
    answered.insert("x-seen", "yes".parse().unwrap());
    (answered, format!("{verb} {uri} {seen} {body}"))
}

async fn names(headers: HeaderMap) -> (HeaderMap, String) {
    let mut answered = HeaderMap::new();
    for (name, value) in [
        ("connection", "x-hop"),
        ("keep-alive", "timeout=5"),
        ("x-hop", "1"),
        ("x-kept", "yes"),
    ] {
        answered.insert(HeaderName::from_static(name), value.parse().unwrap());
    }
    let mut seen: Vec<&str> = headers.keys().map(HeaderName::as_str).collect();
    seen.sort_unstable();
    (answered, seen.join(" "))
}

async fn target() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/moved",
            any(|| async { Redirect::temporary("/elsewhere") }),
        )
        .route("/names", any(names))
        .fallback(any(echo));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

fn config(base: &str, reach: Reach) -> Config {
    let upstream = Upstream {
        base: Url::parse(base).unwrap(),
        secrets: vec![("x-api-key".to_owned(), "gateway-key".to_owned())],
    };
    Config {
        relay: true,
        http: true,
        reach,
        services: BTreeMap::from([("ccs".to_owned(), upstream)]),
        crosspond: BTreeMap::new(),
    }
}

fn served(config: Config) -> Serving {
    let clients = Clients::new(config.reach).unwrap();
    Serving { config, clients }
}

const CALLER: Caller<'static> = Caller {
    chain: "dognet-b5b6ea90",
    program: "harness",
    id: 7,
};

fn post(service: &str, target: &str, headers: Vec<(&str, &str)>) -> Request {
    Request::Http {
        service: service.into(),
        head: Head {
            method: Method::Post,
            target: target.into(),
            headers: headers
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        },
        body: Vec::new(),
    }
}

struct Relayed;

impl Crosspond for Relayed {
    async fn relay(
        &mut self,
        name: &str,
        peer: &Peer,
        link: &NetworkId,
        _: &mut (impl Blocks + Send),
    ) -> Answer {
        let relayed = format!("{name} {} {}", peer.base, abi::hex(link));
        Answer::Output(relayed.into_bytes())
    }
}

async fn ask(serving: &Serving, request: &Request, body: &[u8]) -> Answer {
    ask_at(serving, request, body, &mut Unretried).await
}

async fn ask_at(
    serving: &Serving,
    request: &Request,
    body: &[u8],
    blocks: &mut (impl Blocks + Send),
) -> Answer {
    let delivery = Delivery::Whole;
    let unstreamed = |data| panic!("a whole request streamed {data:?}");
    let called = answer(
        serving,
        &CALLER,
        request,
        &delivery,
        body.to_vec(),
        blocks,
        &mut Relayed,
        unstreamed,
    )
    .await;
    let Called::Whole(answer) = called else {
        panic!("a whole request ended as a stream: {called:?}");
    };
    answer
}

fn output(answer: Answer) -> Response {
    let Answer::Output(bytes) = answer else {
        panic!("answered {answer:?}");
    };
    abi::decode(&bytes).unwrap()
}

fn text(response: Response) -> String {
    String::from_utf8(response.body).unwrap()
}

#[tokio::test]
async fn a_service_request_reaches_its_base_with_the_secret_and_who_asked() {
    let base = target().await;
    let config = config(&format!("{base}/api"), Reach::Public);
    let serving = served(config);
    let request = post("ccs", "/v1/messages?beta=true", Vec::new());
    let response = output(ask(&serving, &request, b"{\"a\":1}").await);
    assert_eq!(response.status, 200);
    assert!(response.headers.contains(&("x-seen".into(), "yes".into())));
    assert_eq!(
        text(response),
        "POST /api/v1/messages?beta=true gateway-key dognet-b5b6ea90 harness 7 {\"a\":1}"
    );
}

#[tokio::test]
async fn a_service_request_that_climbs_out_sets_a_secret_or_names_an_unserved_service_fails() {
    let base = target().await;
    let config = config(&format!("{base}/api"), Reach::Public);
    let serving = served(config);
    let failures = [
        post("ccs", "/%2e%2e/admin", Vec::new()),
        post("ccs", "/v1/messages", vec![("x-api-key", "theirs")]),
        post("other", "/v1/messages", Vec::new()),
        post(RELAY, "/", Vec::new()),
    ];
    for request in failures {
        let failed = ask(&serving, &request, b"").await;
        assert!(
            matches!(failed, Answer::Failed(_)),
            "{request:?}: {failed:?}"
        );
    }
    assert!(join(&Url::parse(&format!("{base}/api")).unwrap(), "x").is_err());
}

#[tokio::test]
async fn the_relay_answers_empty_and_a_node_answers_only_what_it_serves() {
    let base = target().await;
    let mut serving = served(config(&base, Reach::Private));
    let relay = Request::Relay {
        note: b"toolu_1".to_vec(),
    };
    assert_eq!(ask(&serving, &relay, b"").await, Answer::Output(Vec::new()));
    let crosspond = Request::Crosspond { link: [5; 32] };
    let unserved = ask(&serving, &crosspond, b"").await;
    assert!(matches!(unserved, Answer::Failed(_)), "{unserved:?}");
    let peer = Peer {
        network: [5; 32],
        base: Url::parse(&base).unwrap(),
    };
    serving.config.crosspond.insert("pond".into(), peer);
    let relayed = format!("pond {base}/ {}", "05".repeat(32));
    assert_eq!(
        ask(&serving, &crosspond, b"").await,
        Answer::Output(relayed.into_bytes())
    );
    serving.config.relay = false;
    serving.config.http = false;
    let internet = post(HTTP, &format!("{base}/v1/messages"), Vec::new());
    for request in [relay, internet] {
        let unserved = ask(&serving, &request, b"").await;
        assert!(matches!(unserved, Answer::Failed(_)), "{unserved:?}");
    }
}

#[tokio::test]
async fn an_internet_request_gets_no_secret_and_its_redirect_comes_back_unfollowed() {
    let base = target().await;
    let config = config(&base, Reach::Private);
    let serving = served(config);
    let request = post(HTTP, &format!("{base}/v1/messages"), Vec::new());
    let response = output(ask(&serving, &request, b"").await);
    assert_eq!(text(response), "POST /v1/messages - - - - ");
    let moved = post(HTTP, &format!("{base}/moved"), Vec::new());
    let response = output(ask(&serving, &moved, b"").await);
    assert_eq!(response.status, 307);
}

#[tokio::test]
async fn the_node_forwards_end_to_end_headers_only_as_a_proxy_does() {
    let base = target().await;
    let config = config(&base, Reach::Private);
    let serving = served(config);
    let hops = vec![
        ("keep-alive", "1"),
        ("te", "trailers"),
        ("upgrade", "h2c"),
        ("proxy-authorization", "x"),
        ("x-end", "1"),
    ];
    let request = post(HTTP, &format!("{base}/names"), hops);
    let response = output(ask(&serving, &request, b"").await);
    let names: Vec<&str> = response
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(names.contains(&"x-kept"), "{names:?}");
    for hop in ["connection", "keep-alive", "x-hop", "transfer-encoding"] {
        assert!(!names.contains(&hop), "{hop} came back: {names:?}");
    }
    let forwarded = text(response);
    assert!(forwarded.contains("x-end"), "{forwarded}");
    for hop in ["keep-alive", "te", "upgrade", "proxy-authorization"] {
        let sent = forwarded.split(' ').any(|name| name == hop);
        assert!(!sent, "{hop} was forwarded: {forwarded}");
    }
}

#[tokio::test]
async fn a_public_node_refuses_a_private_address_by_literal_or_by_name() {
    let base = target().await;
    let port = base.rsplit(':').next().unwrap();
    let config = config(&base, Reach::Public);
    let serving = served(config);
    for url in [
        format!("{base}/v1/messages"),
        format!("http://[::1]:{port}/v1/messages"),
        format!("http://localhost:{port}/v1/messages"),
        "http://169.254.169.254/latest/meta-data/".to_owned(),
    ] {
        let refused = ask(&serving, &post(HTTP, &url, Vec::new()), b"").await;
        let Answer::Failed(sentence) = refused else {
            panic!("{url}: {refused:?}");
        };
        assert!(
            sentence.ends_with("is not a public address"),
            "{url}: {sentence}"
        );
    }
}

#[test]
fn only_a_routable_unicast_address_is_public() {
    for address in ["93.184.216.34", "2606:4700::1111"] {
        assert!(public(address.parse().unwrap()), "{address}");
    }
    for address in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "255.255.255.255",
        "224.0.0.1",
        "::1",
        "::",
        "fd00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
    ] {
        assert!(!public(address.parse().unwrap()), "{address}");
    }
}

struct Chain {
    tip: u64,
    expiry: u64,
    listens: Option<(u64, SocketAddr)>,
}

impl Blocks for Chain {
    fn tip(&mut self) -> u64 {
        self.tip
    }

    async fn expiry(&mut self) -> Option<u64> {
        Some(self.expiry)
    }

    async fn changed(&mut self) -> bool {
        self.tip += 1;
        if let Some((height, address)) = self.listens
            && height == self.tip
        {
            let listener = TcpListener::bind(address).await.unwrap();
            let app = Router::new().fallback(any(echo));
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        }
        true
    }
}

fn closed() -> SocketAddr {
    Bound::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

#[tokio::test]
async fn a_refused_connection_is_tried_again_at_each_block_until_the_service_listens() {
    let address = closed();
    let config = config(&format!("http://{address}/api"), Reach::Private);
    let serving = served(config);
    let request = post("ccs", "/v1/messages", Vec::new());
    let mut blocks = Chain {
        tip: 10,
        expiry: 74,
        listens: Some((13, address)),
    };
    let answered = ask_at(&serving, &request, b"{}", &mut blocks).await;
    assert_eq!(blocks.tip, 13);
    assert_eq!(
        text(output(answered)),
        "POST /api/v1/messages gateway-key dognet-b5b6ea90 harness 7 {}"
    );
}

#[tokio::test]
async fn a_service_that_never_listens_fails_with_the_refusal_in_the_last_block_before_its_expiry() {
    let address = closed();
    let serving = served(config(&format!("http://{address}"), Reach::Private));
    let request = post("ccs", "/v1/messages", Vec::new());
    let mut blocks = Chain {
        tip: 10,
        expiry: 74,
        listens: None,
    };
    let Answer::Failed(why) = ask_at(&serving, &request, b"", &mut blocks).await else {
        panic!("a closed port answered");
    };
    assert!(why.contains("refused"), "{why}");
    assert_eq!(blocks.tip + 1, blocks.expiry - 1);
}

#[tokio::test]
async fn a_public_url_to_a_closed_port_fails_at_once() {
    let address = closed();
    let serving = served(config(&format!("http://{address}"), Reach::Private));
    let request = post(HTTP, &format!("http://{address}/v1/messages"), Vec::new());
    let mut blocks = Chain {
        tip: 10,
        expiry: 74,
        listens: Some((11, address)),
    };
    let Answer::Failed(why) = ask_at(&serving, &request, b"", &mut blocks).await else {
        panic!("a closed port answered");
    };
    assert!(why.contains("refused"), "{why}");
    assert_eq!(blocks.tip, 10);
}

#[tokio::test]
async fn a_refused_service_whose_expiry_is_unknown_fails_at_once() {
    let address = closed();
    let serving = served(config(&format!("http://{address}"), Reach::Private));
    let request = post("ccs", "/v1/messages", Vec::new());
    let Answer::Failed(why) = ask(&serving, &request, b"").await else {
        panic!("a closed port answered");
    };
    assert!(why.contains("refused the connection after "), "{why}");
}

#[tokio::test]
async fn a_base_that_takes_the_connection_and_never_answers_fails_its_reach_at_the_wait() {
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = Url::parse(&format!("http://{}", silent.local_addr().unwrap())).unwrap();
    let clients = Clients::new(Reach::Public).unwrap();
    let reached = clients.reach(&base, Duration::from_millis(1)).await;
    let Reached::Failed(why) = reached else {
        panic!("{reached:?}");
    };
    let said = format!("{base} sent no answer in ");
    assert!(why.starts_with(&said), "{why}");
    assert!(
        why.ends_with(" ms, the 1 ms egress waits for a request"),
        "{why}"
    );
}

#[test]
fn an_unanswered_call_says_what_failed_and_after_how_long() {
    use std::io::ErrorKind;
    let kinds = [
        (Some(ErrorKind::TimedOut), true, Unanswered::Silent),
        (
            Some(ErrorKind::ConnectionRefused),
            false,
            Unanswered::Refused,
        ),
        (Some(ErrorKind::ConnectionReset), false, Unanswered::Reset),
        (Some(ErrorKind::ConnectionAborted), false, Unanswered::Reset),
        (None, true, Unanswered::Waited),
        (Some(ErrorKind::Other), false, Unanswered::Other),
    ];
    for (kind, timeout, read) in kinds {
        assert_eq!(unanswered(kind, timeout), read, "{kind:?} {timeout}");
    }
    let base = Url::parse("http://10.43.80.80:8080/").unwrap();
    let took = Duration::from_millis(30510);
    let wait = Some(Duration::from_millis(64000));
    assert_eq!(
        said(
            &base,
            Unanswered::Silent,
            took,
            wait,
            "Connection timed out"
        ),
        "http://10.43.80.80:8080/ timed out after 30510 ms: nothing answered, not even a refusal: Connection timed out"
    );
    assert_eq!(
        said(&base, Unanswered::Waited, took, wait, "operation timed out"),
        "http://10.43.80.80:8080/ sent no answer in 30510 ms, the 64000 ms egress waits for a request"
    );
    assert_eq!(
        said(&base, Unanswered::Reset, took, None, "reset by peer"),
        "http://10.43.80.80:8080/ reset the connection after 30510 ms: reset by peer"
    );
    assert_eq!(
        said(&base, Unanswered::Other, took, None, "dns"),
        "the target was not reached after 30510 ms: dns"
    );
}
