use abi::role::reads::{Entry, Op, Read, Row, State};
use abi::{BlobId, Refusal};
use borsh::{BorshDeserialize, BorshSerialize};

#[derive(BorshSerialize, BorshDeserialize)]
enum OrdinaryOp {
    Read {
        target: String,
        query: Vec<u8>,
    },
    Answer {
        id: u64,
        at: u64,
        outcome: Result<Entry, Refusal>,
    },
    Ready {
        id: u64,
    },
}

#[derive(BorshSerialize, BorshDeserialize)]
enum OrdinaryState {
    Asked,
    Putting { bundle: u64, read: Read },
}

#[derive(BorshSerialize, BorshDeserialize)]
struct OrdinaryRow {
    requester: String,
    target: String,
    query: Vec<u8>,
    made: u64,
    state: OrdinaryState,
}

#[test]
fn ordinary_read_answer_ready_and_parked_rows_keep_their_exact_codec() {
    let entry = Entry {
        id: BlobId::Sha256([7; 32]),
        len: 19,
        kind: "read".into(),
    };
    let cases = [
        (
            Op::Read {
                target: "oracle".into(),
                query: vec![1, 2],
            },
            OrdinaryOp::Read {
                target: "oracle".into(),
                query: vec![1, 2],
            },
        ),
        (
            Op::Answer {
                id: 31,
                at: 5,
                outcome: Ok(entry.clone()),
            },
            OrdinaryOp::Answer {
                id: 31,
                at: 5,
                outcome: Ok(entry.clone()),
            },
        ),
        (Op::Ready { id: 31 }, OrdinaryOp::Ready { id: 31 }),
    ];
    for (current, ordinary) in cases {
        let bytes = abi::encode(&ordinary);
        assert_eq!(abi::encode(&current), bytes);
        assert_eq!(abi::decode::<Op>(&bytes).unwrap(), current);
        assert_eq!(
            abi::encode(&abi::decode::<OrdinaryOp>(&bytes).unwrap()),
            bytes
        );
    }
    let read = Read { entry, at: 5 };
    for (state, ordinary) in [
        (State::Asked, OrdinaryState::Asked),
        (
            State::Putting {
                bundle: 9,
                read: read.clone(),
            },
            OrdinaryState::Putting { bundle: 9, read },
        ),
    ] {
        let row = Row {
            requester: "caller".into(),
            target: "oracle".into(),
            query: vec![2],
            made: 3,
            state,
        };
        let old = OrdinaryRow {
            requester: row.requester.clone(),
            target: row.target.clone(),
            query: row.query.clone(),
            made: row.made,
            state: ordinary,
        };
        assert_eq!(abi::encode(&row), abi::encode(&old));
        assert_eq!(abi::decode::<Row>(&abi::encode(&old)).unwrap(), row);
    }
}
