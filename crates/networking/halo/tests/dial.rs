// A dial to a member whose halo address takes the packets and never answers: QUIC's own idle
// timeout ends it.

use std::sync::Arc;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use halo::Members;
use halo::quic::{Error, Quic};

#[tokio::test(start_paused = true)]
async fn a_dial_to_a_member_that_never_answers_ends_when_quic_times_it_out() {
    let identity = PrivateKey::from_seed(1);
    let silent = PrivateKey::from_seed(2).public_key();
    let quic = Quic::bind(
        "127.0.0.1:0".parse().unwrap(),
        &identity,
        Arc::new(Members::default()),
    )
    .unwrap();
    let never_answers = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let at = never_answers.local_addr().unwrap();
    let dialed = quic.dial(&silent, at).await;
    let timed_out = matches!(
        dialed,
        Err(Error::Connection(quinn::ConnectionError::TimedOut))
    );
    assert!(timed_out, "{dialed:?}");
    never_answers.set_nonblocking(true).unwrap();
    let mut packet = [0u8; 2048];
    let heard = never_answers.recv(&mut packet).is_ok();
    assert!(heard, "the dial reached the member that never answered");
}
