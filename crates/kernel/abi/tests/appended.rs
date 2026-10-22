use abi::role::access::Party;
use abi::role::egress::{self, Request};
use abi::{BlobId, CryptoOp, HashKind, HostOp, Message, Principal, Scan};

fn framed(bytes: &[u8]) -> Vec<u8> {
    let len = u32::try_from(bytes.len()).unwrap();
    [len.to_le_bytes().as_slice(), bytes].concat()
}

fn number(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

fn tagged(tag: u8, fields: &[&[u8]]) -> Vec<u8> {
    [&[tag][..], &fields.concat()].concat()
}

#[test]
fn every_existing_principal_keeps_its_bytes() {
    assert_eq!(
        abi::encode(&Principal::Account(7)),
        tagged(0, &[&number(7)])
    );
    assert_eq!(abi::encode(&Principal::System), [1]);
}

#[test]
fn every_existing_party_keeps_its_bytes() {
    assert_eq!(abi::encode(&Party::Account(7)), tagged(0, &[&number(7)]));
    assert_eq!(
        abi::encode(&Party::Program("p".into())),
        tagged(1, &[&framed(b"p")])
    );
}

#[test]
fn every_existing_egress_request_and_reserved_service_keeps_its_bytes() {
    let relay = Request::Relay { note: vec![7] };
    assert_eq!(abi::encode(&relay), tagged(0, &[&framed(&[7])]));
    let http = Request::Http {
        service: "s".into(),
        head: egress::http::Head {
            method: egress::http::Method::Get,
            target: "/".into(),
            headers: Vec::new(),
        },
        body: vec![BlobId::Sha256([9; 32])],
    };
    let head = [&[0u8][..], &framed(b"/"), &framed(&[])].concat();
    let body = [&1u32.to_le_bytes()[..], &[0], &[9; 32]].concat();
    assert_eq!(
        abi::encode(&http),
        tagged(1, &[&framed(b"s"), &head, &body])
    );
    assert_eq!((egress::HTTP, egress::RELAY), ("http", "relay"));
    assert_eq!(egress::RESERVED[..2], [egress::HTTP, egress::RELAY]);
}

#[test]
fn every_existing_host_op_keeps_its_tag_and_bytes() {
    let key = || vec![7u8];
    let sha1 = BlobId::Sha1([9; 20]);
    let sha256 = BlobId::Sha256([9; 32]);
    let sha1_bytes = [&[1u8][..], &[9; 20]].concat();
    let sha256_bytes = [&[0u8][..], &[9; 32]].concat();
    let message = Message {
        target: "p".into(),
        payload: key(),
        reply: true,
        acting: Some(3),
    };
    let message_bytes = [&framed(b"p")[..], &framed(&[7]), &[1, 1], &number(3)].concat();
    let cases: Vec<(HostOp, Vec<u8>)> = vec![
        (HostOp::Get(key()), tagged(0, &[&framed(&[7])])),
        (
            HostOp::Set {
                key: key(),
                value: vec![8],
            },
            tagged(1, &[&framed(&[7]), &framed(&[8])]),
        ),
        (HostOp::Delete(key()), tagged(2, &[&framed(&[7])])),
        (
            HostOp::Scan(Scan::range(b"k".to_vec(), Some(b"l".to_vec()))),
            tagged(3, &[&framed(b"k"), &[1], &framed(b"l"), &[0, 0]]),
        ),
        (HostOp::CommittedGet(key()), tagged(4, &[&framed(&[7])])),
        (
            HostOp::CommittedScan(Scan::range(Vec::new(), None).reverse().limit(2)),
            tagged(5, &[&framed(&[]), &[0, 1, 1], &number(2)]),
        ),
        (
            HostOp::BlobPutAlone {
                hash: HashKind::Sha256,
                kind: "k".into(),
                body: key(),
            },
            tagged(6, &[&[0], &framed(b"k"), &framed(&[7])]),
        ),
        (HostOp::BlobGet(sha1), tagged(7, &[&sha1_bytes])),
        (HostOp::BlobStat(sha256), tagged(8, &[&sha256_bytes])),
        (
            HostOp::BlobRead {
                id: sha1,
                offset: 1,
                len: 2,
            },
            tagged(9, &[&sha1_bytes, &number(1), &number(2)]),
        ),
        (HostOp::Root("p".into()), tagged(10, &[&framed(b"p")])),
        (
            HostOp::Query {
                program: "p".into(),
                request: key(),
            },
            tagged(11, &[&framed(b"p"), &framed(&[7])]),
        ),
        (HostOp::Emit(message), tagged(12, &[&message_bytes])),
        (HostOp::Event(key()), tagged(13, &[&framed(&[7])])),
        (HostOp::Output(key()), tagged(14, &[&framed(&[7])])),
        (HostOp::Respond(key()), tagged(15, &[&framed(&[7])])),
        (
            HostOp::Crypto(CryptoOp::Sha256(key())),
            tagged(16, &[&[0], &framed(&[7])]),
        ),
        (
            HostOp::Await {
                target: "p".into(),
                payload: key(),
                acting: None,
            },
            tagged(17, &[&framed(b"p"), &framed(&[7]), &[0]]),
        ),
        (HostOp::Defer, vec![18]),
        (
            HostOp::Resume {
                frame: 1,
                ticket: 2,
                answer: Ok(key()),
            },
            tagged(19, &[&number(1), &number(2), &[1], &framed(&[7])]),
        ),
        (
            HostOp::Resource {
                program: "p".into(),
                key: "k".into(),
            },
            tagged(20, &[&framed(b"p"), &framed(b"k")]),
        ),
        (
            HostOp::SetOwned {
                key: key(),
                value: vec![8],
                kind: "k".into(),
            },
            tagged(21, &[&framed(&[7]), &framed(&[8]), &framed(b"k")]),
        ),
        (HostOp::BlobRelease(sha256), tagged(22, &[&sha256_bytes])),
        (
            HostOp::BlobPut {
                hash: HashKind::Sha1,
                kind: "k".into(),
                body: key(),
                line: vec![8],
            },
            tagged(23, &[&[1], &framed(b"k"), &framed(&[7]), &framed(&[8])]),
        ),
        (
            HostOp::BlobReleaseLines { prefix: key() },
            tagged(24, &[&framed(&[7])]),
        ),
        (HostOp::Client, vec![25]),
    ];
    for (op, bytes) in cases {
        assert_eq!(abi::encode(&op), bytes, "{op:?}");
        assert_eq!(abi::decode::<HostOp>(&bytes).unwrap(), op);
    }
}

#[test]
fn each_appended_variant_encodes_at_the_tag_after_the_last_existing_one() {
    let foreign = abi::Foreign {
        network: [5; 32],
        account: 7,
    };
    let foreign_bytes = [&[5u8; 32][..], &number(7)].concat();
    assert_eq!(
        abi::encode(&Principal::Foreign(foreign)),
        tagged(2, &[&foreign_bytes])
    );
    assert_eq!(
        abi::encode(&Party::Foreign(foreign)),
        tagged(2, &[&foreign_bytes])
    );
    let crosspond = Request::Crosspond { link: [5; 32] };
    assert_eq!(abi::encode(&crosspond), tagged(2, &[&[5; 32]]));
    assert_eq!(crosspond.service(), egress::CROSSPOND);
    assert!(crosspond.parts().is_empty());
    assert_eq!(
        egress::RESERVED,
        [egress::HTTP, egress::RELAY, egress::CROSSPOND]
    );
    assert_eq!(egress::CROSSPOND, "crosspond");
    let message = Message {
        target: "p".into(),
        payload: vec![7],
        reply: true,
        acting: None,
    };
    let message_bytes = [&framed(b"p")[..], &framed(&[7]), &[1, 0]].concat();
    let emit = HostOp::EmitForeign { message, foreign };
    assert_eq!(
        abi::encode(&emit),
        tagged(26, &[&message_bytes, &foreign_bytes])
    );
    assert_eq!(abi::decode::<HostOp>(&abi::encode(&emit)).unwrap(), emit);
    assert_eq!(emit.as_put(), None);
}
