use std::sync::Arc;

use axum::http::StatusCode;
use futures::StreamExt as _;
use noded::Client;
use statesync::{Exchange as _, Failure, Fault as _, Request, Response};
use tokio::net::TcpListener;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

async fn failure_of(base: String) -> Failure {
    let asked = Client::new(base).exchange(Request::Head).await;
    asked.expect_err("the source answered").failure()
}

async fn answering(status: StatusCode) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().fallback(move || async move { status });
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

#[test]
fn a_server_error_is_asked_again() {
    runtime().block_on(async {
        let base = answering(StatusCode::BAD_GATEWAY).await;
        assert_eq!(failure_of(base).await, Failure::Transient("source_failed"));
    });
}

#[test]
fn a_busy_source_is_asked_again() {
    runtime().block_on(async {
        let base = answering(StatusCode::TOO_MANY_REQUESTS).await;
        assert_eq!(failure_of(base).await, Failure::Transient("source_busy"));
    });
}

#[test]
fn a_route_the_source_does_not_serve_is_definite() {
    runtime().block_on(async {
        let base = answering(StatusCode::NOT_FOUND).await;
        assert_eq!(failure_of(base).await, Failure::Definite);
    });
}

#[test]
fn a_refused_connection_is_asked_again() {
    runtime().block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let failure = failure_of(base).await;
        assert_eq!(failure, Failure::Transient("source_unreachable"));
    });
}

#[test]
fn a_connection_dropped_before_the_answer_is_asked_again() {
    runtime().block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                drop(stream);
            }
        });
        assert_eq!(failure_of(base).await, Failure::Transient("source_dropped"));
    });
}

#[test]
fn a_certificate_that_does_not_verify_is_definite() {
    runtime().block_on(async {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key =
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key.into())
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("https://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                drop(acceptor.accept(stream).await);
            }
        });
        assert_eq!(failure_of(base).await, Failure::Definite);
    });
}

async fn streaming(chunks: Vec<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().fallback(move || {
        let chunks = chunks.clone();
        async move {
            let parts = chunks.into_iter().map(Ok::<_, std::io::Error>);
            axum::body::Body::from_stream(futures::stream::iter(parts))
        }
    });
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

async fn streamed(base: String, asked: usize) -> Vec<Result<Option<Vec<u8>>, Failure>> {
    let ids = vec![abi::BlobId::Sha1([0; 20]); asked];
    Client::new(base)
        .stream_blobs(ids)
        .map(|read| read.map_err(|error| error.failure()))
        .collect()
        .await
}

#[test]
fn a_blob_answer_read_in_pieces_gives_each_blob_whole() {
    runtime().block_on(async {
        let page = vec![
            Some(b"first".to_vec()),
            None,
            Some(Vec::new()),
            Some(vec![7; 300]),
        ];
        let bytes = abi::encode(&page);
        let chunks = bytes.chunks(3).map(<[u8]>::to_vec).collect();
        let read = streamed(streaming(chunks).await, page.len()).await;
        let expected: Vec<_> = page.into_iter().map(Ok).collect();
        assert_eq!(read, expected);
    });
}

#[test]
fn a_blob_answer_cut_short_is_asked_again() {
    runtime().block_on(async {
        let page = vec![Some(b"first".to_vec()), Some(b"second".to_vec())];
        let mut bytes = abi::encode(&page);
        bytes.truncate(bytes.len() - 3);
        let read = streamed(streaming(vec![bytes]).await, page.len()).await;
        let first = Ok(Some(b"first".to_vec()));
        let cut = Err(Failure::Transient("source_dropped"));
        assert_eq!(read, vec![first, cut]);
    });
}

async fn hold_failure_of(base: String) -> Failure {
    match Client::new(base).hold().await {
        Ok(_) => panic!("the source held a head"),
        Err(error) => error.failure(),
    }
}

#[test]
fn a_hold_the_source_fails_is_asked_again() {
    runtime().block_on(async {
        let base = answering(StatusCode::BAD_GATEWAY).await;
        let failure = hold_failure_of(base).await;
        assert_eq!(failure, Failure::Transient("source_failed"));
    });
}

#[test]
fn a_hold_whose_connection_is_refused_is_asked_again() {
    runtime().block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let failure = hold_failure_of(base).await;
        assert_eq!(failure, Failure::Transient("source_unreachable"));
    });
}

#[test]
fn a_hold_on_a_route_the_source_does_not_serve_is_definite() {
    runtime().block_on(async {
        let base = answering(StatusCode::NOT_FOUND).await;
        assert_eq!(hold_failure_of(base).await, Failure::Definite);
    });
}

#[test]
fn a_hold_answered_without_a_whole_head_is_definite() {
    runtime().block_on(async {
        let page = b"<!DOCTYPE html><html><body>a proxy's page</body></html>".to_vec();
        for answer in [Vec::new(), page] {
            let base = streaming(vec![answer]).await;
            assert_eq!(hold_failure_of(base).await, Failure::Definite);
        }
    });
}

#[test]
fn a_hold_whose_head_arrives_in_pieces_takes_it_whole() {
    runtime().block_on(async {
        let refusal = abi::Refusal::new(abi::reason::NOT_FOUND, "a head sent in pieces");
        let encoded = abi::encode(&Response::Refused(refusal));
        let mut framed = (encoded.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(&encoded);
        let chunks = framed.chunks(3).map(<[u8]>::to_vec).collect();
        let base = streaming(chunks).await;
        let held = Client::new(base).hold().await.unwrap();
        assert_eq!(abi::encode(&held.head), encoded);
        assert_eq!(
            held.hold, None,
            "an answer with no hold header names no hold"
        );
    });
}

#[test]
fn a_hold_reads_each_line_after_its_head_and_ends_when_its_answer_does() {
    runtime().block_on(async {
        let refusal = abi::Refusal::new(abi::reason::NOT_FOUND, "a head and its lines");
        let encoded = abi::encode(&Response::Refused(refusal));
        let mut framed = (encoded.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(&encoded);
        let mut chunks = vec![framed];
        for height in [7u64, 8] {
            let mut line = 8u32.to_le_bytes().to_vec();
            line.extend_from_slice(&height.to_le_bytes());
            chunks.push(line);
        }
        let base = streaming(chunks).await;
        let held = Client::new(base).hold().await.unwrap();
        held.ending.await;
    });
}

#[test]
fn a_hold_takes_its_number_from_the_answer_header() {
    runtime().block_on(async {
        let refusal = abi::Refusal::new(abi::reason::NOT_FOUND, "a numbered head");
        let encoded = abi::encode(&Response::Refused(refusal));
        let mut framed = (encoded.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(&encoded);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = axum::Router::new().fallback(move || {
            let framed = framed.clone();
            async move { ([("ducktape-hold", "42")], framed) }
        });
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let held = Client::new(base).hold().await.unwrap();
        assert_eq!(abi::encode(&held.head), encoded);
        assert_eq!(held.hold, Some(42));
    });
}
