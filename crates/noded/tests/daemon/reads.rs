// Reads a node answers off the block path: the query run on the member, its reply as a blob.

use super::*;

use abi::role::awaits::Answer;
use abi::role::reads;
use fixture_awaiter::{ANSWERS, Plan};

const ASKER_SEED: u64 = 41;

#[test]
fn a_node_answers_a_read_with_the_blob_of_the_bytes_its_query_answers_and_resumes_the_caller() {
    apart!();
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding_with(
        root.path(),
        &[&seat],
        EPOCH_LENGTH,
        MEMBER_CAP,
        &[("awaiter", AWAITER), ("oracle", ORACLE)],
    );
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();

    client_runtime().block_on(async {
        let key = ed25519::PrivateKey::from_seed(ASKER_SEED);
        let query = b"no such key".to_vec();
        let asked = query_frame(&client, &key, "oracle", query.clone()).await;
        let direct = client.query(Layer::Confirmed, asked).await.unwrap();

        let mut answers = client.changes("awaiter").await.unwrap();
        let read = reads::Op::Read {
            target: "oracle".into(),
            query,
        };
        let plan = Plan {
            target: reads::PROGRAM.into(),
            acting: None,
            asks: vec![abi::encode(&read)],
            together: false,
            kept: 0,
            fail: false,
        };
        let frame = signed(&client, &key, "awaiter", abi::encode(&plan)).await;
        let (_, receipt) = client.land(&Frame::decode(&frame).unwrap()).await.unwrap();
        assert!(
            matches!(receipt.outcome, Outcome::Applied { .. }),
            "{receipt:?}"
        );
        landed(&mut answers, &[ANSWERS]).await;

        let kept = client
            .get(Layer::Confirmed, "awaiter", ANSWERS)
            .await
            .unwrap()
            .expect("the awaiter kept its answer");
        let kept: Vec<Answer> = abi::decode(&kept).unwrap();
        let [Ok(entry)]: [Answer; 1] = kept.try_into().unwrap() else {
            panic!("the read is answered with its blob");
        };
        let read: reads::Read = abi::decode(&entry).unwrap();
        let entry = read.entry;
        let framed = blob::frame(reads::KIND, &direct).unwrap();
        assert_eq!(entry.id, blob::id_of(HashKind::Sha256, &framed).unwrap());
        assert_eq!(entry.len, framed.len() as u64);
        assert_eq!(entry.kind, reads::KIND);
        assert!(
            client.blob(entry.id).await.unwrap().is_some(),
            "the node serves the read's blob"
        );
        assert!(
            client
                .scan(Layer::Confirmed, host::PARKED, Scan::prefix(b"read/"))
                .await
                .unwrap()
                .is_empty(),
            "no read is left open"
        );
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}
