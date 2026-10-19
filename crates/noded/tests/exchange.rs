use std::sync::Arc;

use axum::http::StatusCode;
use noded::Client;
use statesync::{Exchange as _, Failure, Fault as _, Request};
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
