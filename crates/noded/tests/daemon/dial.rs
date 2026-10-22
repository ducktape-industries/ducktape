// The p2p schemes a validator is dialed at.

use super::*;

/// Ends TLS at `listen` and carries the bytes to `to`, as a tunnel or a
/// reverse proxy in front of a node's HTTP port does.
fn terminate_tls(runtime: &::tokio::runtime::Runtime, listen: TcpListener, to: SocketAddr) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let server = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certified.cert.der().clone()], key.into())
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server));
    runtime.spawn(async move {
        listen.set_nonblocking(true).unwrap();
        let listener = ::tokio::net::TcpListener::from_std(listen).unwrap();
        loop {
            let (remote, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            ::tokio::spawn(async move {
                let Ok(mut remote) = acceptor.accept(remote).await else {
                    return;
                };
                let mut node = ::tokio::net::TcpStream::connect(to).await.unwrap();
                let _ = ::tokio::io::copy_bidirectional(&mut remote, &mut node).await;
            });
        }
    });
}

fn dialed_at(address: impl FnOnce(&Seat) -> String) {
    let root = tempfile::tempdir().unwrap();
    let mut dialer = Seat::new(root.path(), "n0");
    let mut dialed = Seat::new(root.path(), "n1");
    dialer.address = None;
    dialed.address = Some(address(&dialed));
    reached(&client_runtime(), root.path(), dialer, dialed);
}

#[test]
fn a_validator_is_dialed_over_tcp_at_its_name() {
    apart!();
    dialed_at(|seat| format!("tcp://localhost:{}", seat.p2p.port()));
}

#[test]
fn a_validator_is_dialed_over_tls() {
    apart!();
    dialed_at(|seat| format!("tls://localhost:{}", seat.tls.port()));
}

#[test]
fn a_validator_is_dialed_over_a_websocket_on_its_http_port() {
    apart!();
    dialed_at(|seat| format!("ws://localhost:{}", seat.http.port()));
}

#[test]
fn a_validator_is_dialed_over_wss_through_a_tls_terminator() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let runtime = client_runtime();
    let mut dialer = Seat::new(root.path(), "n0");
    let mut dialed = Seat::new(root.path(), "n1");
    let terminator = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = terminator.local_addr().unwrap().port();
    terminate_tls(&runtime, terminator, dialed.http);
    dialer.address = None;
    dialed.address = Some(format!("wss://localhost:{port}"));
    reached(&runtime, root.path(), dialer, dialed);
}
