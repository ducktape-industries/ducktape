use abi::role::crosspond::{
    Acked, CertificateKind, Header, Op, Packet, Proven, ProvenAck, ProvenLow, Query, Reply,
    Settled, StateProof, Status, commitment_key, low_key, settled_key,
};
use abi::{Foreign, Refusal};

fn tag(value: &impl borsh::BorshSerialize) -> u8 {
    abi::encode(value)[0]
}

fn packet() -> Packet {
    Packet {
        source: [1; 32],
        destination: [2; 32],
        seq: 1,
        from: 18,
        name: "chief".into(),
        via: None,
        port: "chat".into(),
        op: vec![7],
        sent: 9,
        deadline: None,
    }
}

fn proof() -> StateProof {
    StateProof {
        location: 3,
        proof: vec![4],
    }
}

#[test]
fn a_network_without_crosspond_still_fills_every_role_it_must() {
    assert_eq!(abi::role::CROSSPOND, "crosspond");
    assert!(!abi::role::REQUIRED.contains(&abi::role::CROSSPOND));
}

#[test]
fn commitment_keys_name_the_destination_and_order_by_sequence_number() {
    let destination = [2; 32];
    let key = commitment_key(&destination, 1);
    assert_eq!(
        key,
        [b"out/".as_slice(), &destination, &1u64.to_be_bytes()].concat()
    );
    let sequence = [1u64, 2, 255, 256, 65_536, u64::from(u32::MAX) + 1, u64::MAX];
    let keys: Vec<Vec<u8>> = sequence
        .iter()
        .map(|seq| commitment_key(&destination, *seq))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(sorted, keys);
    assert_ne!(commitment_key(&[3; 32], 1), key);
}

#[test]
fn settled_and_low_keys_name_their_network_and_order_by_sequence_number() {
    let network = [2; 32];
    assert_eq!(
        settled_key(&network, 1),
        [b"settled/".as_slice(), &network, &1u64.to_be_bytes()].concat()
    );
    assert_eq!(
        low_key(&network, 1),
        [b"low/".as_slice(), &network, &1u64.to_be_bytes()].concat()
    );
    for key in [settled_key, low_key] {
        let keys: Vec<Vec<u8>> = [1u64, 255, 256, u64::MAX]
            .iter()
            .map(|seq| key(&network, *seq))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(sorted, keys);
        assert_ne!(key(&[3; 32], 1), key(&network, 1));
    }
    assert_ne!(settled_key(&network, 1), commitment_key(&network, 1));
}

#[test]
fn a_settled_row_binds_its_packet_and_reads_back_in_an_answer() {
    let settled = Settled {
        source: [1; 32],
        destination: [2; 32],
        seq: 3,
        packet: [4; 32],
        result: Err(Refusal::new("stale", "the deadline passed")),
    };
    let result: Result<Vec<u8>, Refusal> = Err(Refusal::new("stale", "the deadline passed"));
    let fields = [
        [1u8; 32].as_slice(),
        &[2; 32],
        &3u64.to_le_bytes(),
        &[4; 32],
        &abi::encode(&result),
    ]
    .concat();
    assert_eq!(abi::encode(&settled), fields);
    let answer = Acked {
        epoch: 5,
        header: None,
        acks: vec![ProvenAck {
            settled,
            proof: proof(),
        }],
    };
    assert_eq!(abi::decode::<Acked>(&abi::encode(&answer)).unwrap(), answer);
}

#[test]
fn a_proven_delivery_with_no_low_water_and_no_packets_is_exactly_the_deliver_head() {
    use abi::role::crosspond::DELIVER_HEAD;
    let bare = Op::DeliverProven {
        from: [1; 32],
        epoch: u64::MAX,
        low: None,
        packets: Vec::new(),
    };
    assert_eq!(abi::encode(&bare).len(), DELIVER_HEAD);
}

#[test]
fn tag_8_is_retired_and_no_op_decodes_from_it() {
    let mut retired = vec![8u8];
    retired.extend_from_slice(&[1; 32]);
    retired.extend_from_slice(&1u64.to_le_bytes());
    retired.extend_from_slice(&1u64.to_le_bytes());
    retired.extend_from_slice(&0u32.to_le_bytes());
    assert!(abi::decode::<Op>(&retired).is_err());
}

#[test]
fn a_certificate_kind_the_abi_does_not_name_fails_at_its_own_byte() {
    assert_eq!(abi::encode(&CertificateKind::Ed25519SimplexFinalize), [0]);
    let unknown = borsh::from_slice::<CertificateKind>(&[1]).unwrap_err();
    assert_eq!(unknown.kind(), std::io::ErrorKind::InvalidData);
    let header = Header {
        kind: CertificateKind::Ed25519SimplexFinalize,
        block: vec![1],
        certificate: vec![2],
        roots: Vec::new(),
        validators: Vec::new(),
        validators_proof: proof(),
    };
    let mut bytes = abi::encode(&header);
    assert_eq!(bytes[0], 0);
    bytes[0] = 1;
    let refused = borsh::from_slice::<Header>(&bytes).unwrap_err();
    assert_eq!(refused.to_string(), unknown.to_string());
    let alone = borsh::from_slice::<Header>(&[1]).unwrap_err();
    assert_eq!(alone.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(alone.to_string(), unknown.to_string());
    assert!(abi::decode::<Header>(&bytes).is_err());
}

#[test]
fn the_crosspond_role_takes_its_ops_at_their_tags() {
    let network = [1; 32];
    let header = Header {
        kind: CertificateKind::Ed25519SimplexFinalize,
        block: vec![1],
        certificate: vec![2],
        roots: vec![("crosspond".into(), abi::Root([3; 32]))],
        validators: vec![vec![4; 32]],
        validators_proof: proof(),
    };
    let kind = CertificateKind::Ed25519SimplexFinalize;
    let ops = [
        (
            0,
            Op::Open {
                network,
                name: "sealpond".into(),
                epoch_length: 64,
                epoch: 0,
                kind,
                validators: Vec::new(),
            },
        ),
        (
            1,
            Op::Repin {
                network,
                epoch: 1,
                kind,
                validators: Vec::new(),
            },
        ),
        (2, Op::Close { network }),
        (
            3,
            Op::Port {
                role: "chat".into(),
                from: vec![network],
            },
        ),
        (
            4,
            Op::Send {
                to: network,
                port: "chat".into(),
                op: vec![7],
                deadline: None,
            },
        ),
        (
            5,
            Op::SendFor {
                to: network,
                port: "chat".into(),
                op: vec![7],
                deadline: Some(5),
                account: 18,
            },
        ),
        (6, Op::Pump { to: network }),
        (
            7,
            Op::Header {
                from: network,
                header,
            },
        ),
        (
            9,
            Op::DeliverProven {
                from: network,
                epoch: 1,
                low: Some(ProvenLow {
                    n: 2,
                    proof: proof(),
                }),
                packets: vec![Proven {
                    packet: packet(),
                    proof: proof(),
                }],
            },
        ),
    ];
    for (expected, op) in ops {
        assert_eq!(tag(&op), expected, "{op:?}");
        assert_eq!(abi::decode::<Op>(&abi::encode(&op)).unwrap(), op);
    }
    assert_eq!(
        abi::encode(&Op::Close { network }),
        [&[2u8][..], &network].concat()
    );
}

#[test]
fn the_crosspond_role_asks_and_answers_at_its_tags() {
    let network = [1; 32];
    let queries = [
        Query::Link { network },
        Query::Outbound {
            to: network,
            after: 0,
        },
        Query::Receipts {
            from: network,
            after: 0,
        },
        Query::Status {
            to: network,
            seq: 1,
        },
        Query::Ports,
        Query::Name(Foreign {
            network,
            account: 18,
        }),
        Query::Named {
            network,
            name: "chief".into(),
        },
        Query::Home,
        Query::Linked {
            name: "sealpond".into(),
        },
    ];
    for (expected, query) in queries.into_iter().enumerate() {
        assert_eq!(usize::from(tag(&query)), expected, "{query:?}");
        assert_eq!(abi::decode::<Query>(&abi::encode(&query)).unwrap(), query);
    }
    let refused = Refusal::new("invalid_input", "chat: Op did not decode");
    let replies = [
        Reply::Link(None),
        Reply::Outbound(vec![packet()]),
        Reply::Receipts(Vec::new()),
        Reply::Status(Some(Status::Done(Err(refused)))),
        Reply::Ports(Default::default()),
        Reply::Name(Some("chief".into())),
        Reply::Named(vec![18]),
        Reply::Home(network),
        Reply::Linked(vec![network]),
    ];
    for (expected, reply) in replies.into_iter().enumerate() {
        assert_eq!(usize::from(tag(&reply)), expected, "{reply:?}");
        assert_eq!(abi::decode::<Reply>(&abi::encode(&reply)).unwrap(), reply);
    }
    let statuses = [
        Status::Pending,
        Status::Refused {
            reason: "crosspond: Op did not decode".into(),
        },
        Status::Done(Ok(Vec::new())),
    ];
    for (expected, status) in statuses.into_iter().enumerate() {
        assert_eq!(usize::from(tag(&status)), expected, "{status:?}");
    }
}

#[test]
fn a_varint_counts_seven_bits_a_byte() {
    let widths = [0, 1, 127, 128, 16_383, 16_384, 1 << 24, 1 << 62, u64::MAX]
        .map(abi::role::crosspond::varint_len);
    assert_eq!(widths, [1, 1, 1, 2, 2, 3, 4, 9, 10]);
}

#[test]
fn a_packet_to_a_longer_named_network_has_that_much_less_room() {
    use abi::BLOCK_BYTES;
    use abi::role::crosspond::{RELAY_FRAME_ROOM, max_packet};
    assert_eq!(max_packet(""), BLOCK_BYTES - RELAY_FRAME_ROOM);
    assert_eq!(max_packet("sealpond"), max_packet("") - "sealpond".len());
    assert_eq!(max_packet(&"n".repeat(BLOCK_BYTES)), 0);
}

#[test]
fn a_relay_frame_makes_room_for_its_envelopes_target_deliver_fields_and_longest_proof() {
    use abi::role::crosspond::{
        BLOCK_ENVELOPE, DELIVER_FIELDS, FRAME_ENVELOPE, LONGEST_OP_PROOF, RELAY_FRAME_ROOM,
    };
    let terms = [
        BLOCK_ENVELOPE,
        FRAME_ENVELOPE,
        DELIVER_FIELDS,
        LONGEST_OP_PROOF,
        RELAY_FRAME_ROOM,
    ];
    assert_eq!(terms, [115, 159, 58, 3924, 4320]);
}
