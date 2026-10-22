// A node dials the halo address it registered as a member would, and reaches itself there.

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, tokio as runtime};
use halo::{Config, Halo, Queues, Seat};

#[test]
fn a_node_reaches_itself_at_the_halo_address_it_registered() {
    runtime::Runner::default().start(|context| async move {
        let identity = PrivateKey::from_seed(1);
        let key = identity.public_key();
        let config = Config {
            egress_bytes_per_sec: None,
            udp: std::net::UdpSocket::bind("127.0.0.1:0").unwrap().into(),
            reach: halo::Https::shared(),
            queues: Queues::default(),
        };
        let halo = Halo::start(&context.child("node"), identity, config).unwrap();
        let at = format!("localhost:{}", halo.udp().unwrap().port());
        halo.seat([Seat {
            key,
            halo: Some(at.clone()),
            https: None,
            validator: true,
        }]);
        assert_eq!(halo.dial_self(&at).await, Ok(()));
    });
}
