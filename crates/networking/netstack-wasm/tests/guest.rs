//! The wasm guest over the frozen suite: every scenario, against the SAME
//! fixtures the native machine is held to — the traces must match byte for
//! byte — on the guest alone and on both native/guest crossings a swap
//! makes; plus the behaviors the native machine cannot show: a fault, and
//! a snapshot crossing the boundary.

use std::net::{IpAddr, Ipv4Addr};

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use netstack_machine::{Event, Machine, MachineConfig, MeshEpochEvent, NetstackMachine};
use netstack_scenarios::Backend;
use netstack_wasm::{GuestError, NetstackGuest};
use wireguard::{Endpoint, IdentitySigner, PortPolicy, Transport, X25519PublicKey};

/// The canonical artifact, built by guest-builder from the machine crate and
/// committed beside it (`make wasm-modules`).
const COMPONENT: &[u8] = include_bytes!("../../netstack-machine/component.wasm");

fn guest_build(signer: Box<dyn IdentitySigner>, config: MachineConfig) -> Box<dyn NetstackMachine> {
    Box::new(NetstackGuest::new(COMPONENT, signer, config).expect("the netstack component loads"))
}

fn guest_restore(
    signer: Box<dyn IdentitySigner>,
    config: MachineConfig,
    snapshot: &[u8],
) -> Box<dyn NetstackMachine> {
    Box::new(
        NetstackGuest::restore(COMPONENT, signer, config, snapshot)
            .expect("the guest restores the snapshot a scenario took"),
    )
}

/// The guest, building and restoring.
const GUEST: Backend = Backend {
    build: guest_build,
    restore: guest_restore,
};

/// Native machines that a swap replaces with guests.
const NATIVE_TO_GUEST: Backend = Backend {
    build: netstack_scenarios::native_build,
    restore: guest_restore,
};

/// Guests that a swap replaces with native machines.
const GUEST_TO_NATIVE: Backend = Backend {
    build: guest_build,
    restore: netstack_scenarios::native_restore,
};

netstack_scenarios::suite!(GUEST);

mod native_to_guest {
    netstack_scenarios::suite!(super::NATIVE_TO_GUEST);
}

mod guest_to_native {
    netstack_scenarios::suite!(super::GUEST_TO_NATIVE);
}

fn public_node(octet: u8) -> (PrivateKey, MachineConfig) {
    let policy = PortPolicy::production();
    let ip = IpAddr::V4(Ipv4Addr::new(8, 8, 8, octet));
    let endpoint = |port, transport| Endpoint::new(ip, port, transport, &policy).unwrap();
    let signer = PrivateKey::from_seed(u64::from(octet));
    let config = MachineConfig {
        chain_id: "net#guest".into(),
        wireguard_public: X25519PublicKey([octet; 32]),
        wireguard_advertised: Some(endpoint(51_820, Transport::Udp)),
        control_endpoint: endpoint(443, Transport::Tcp),
        coordinators: Vec::new(),
        port_policy: policy.clone(),
        persist: false,
        gossip_ingress: None,
    };
    (signer, config)
}

/// A native machine mid-assembly: retargeted to a two-member epoch with
/// its own record out and nothing back yet.
fn assembling(seed: u8, peer: u8) -> (PrivateKey, MachineConfig, Machine) {
    let (signer, config) = public_node(seed);
    let (peer, _) = public_node(peer);
    let mut machine = Machine::new(Box::new(signer.clone()), config.clone());
    let retarget = Event::Retarget {
        event: MeshEpochEvent {
            epoch: 1,
            members: vec![signer.public_key(), peer.public_key()],
            standbys: Vec::new(),
            current_view: 10,
        },
        persisted: None,
    };
    machine.step(retarget, 1_000).unwrap();
    (signer, config, machine)
}

/// A snapshot crosses the boundary both ways: the guest continues the
/// native machine's epoch, the native machine continues the guest's, and
/// the state is the same bytes on either side of every crossing.
#[test]
fn a_snapshot_crosses_the_boundary_both_ways() {
    let (signer, config, mut native) = assembling(10, 20);
    let taken = native.snapshot().unwrap();

    let mut guest =
        NetstackGuest::restore(COMPONENT, Box::new(signer.clone()), config.clone(), &taken).unwrap();
    assert_eq!(guest.snapshot().unwrap(), taken);

    let from_native = native.step(Event::Nudge, 3_000).unwrap();
    let from_guest = guest.step(Event::Nudge, 3_000).unwrap();
    assert_eq!(from_native, from_guest, "both continue the same epoch");

    let back = Machine::restore(Box::new(signer), config, &guest.snapshot().unwrap()).unwrap();
    assert_eq!(back.snapshot().unwrap(), native.snapshot().unwrap());
}

/// A snapshot from another contract is refused by the guest, by name.
#[test]
fn a_foreign_snapshot_is_refused_by_the_guest() {
    let (signer, config, native) = assembling(10, 20);
    let mut foreign = native.snapshot().unwrap();
    let first_hex = 4;
    foreign[first_hex] = if foreign[first_hex] == b'0' {
        b'1'
    } else {
        b'0'
    };
    let err = match NetstackGuest::restore(COMPONENT, Box::new(signer), config, &foreign) {
        Ok(_) => panic!("a foreign snapshot was accepted"),
        Err(err) => err,
    };
    assert!(
        matches!(&err, GuestError::Restore(reason) if reason.contains("contract")),
        "{err}"
    );
}
