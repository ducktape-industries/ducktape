//! File bodies travel as signed/generic module messages and query pages.
#[path = "fs_support/mod.rs"]
mod support;

use base64::Engine as _;
use duckfs_client::api::NodeApi as _;

#[test]
fn module_transport_round_trips_inline_and_multiple_chunks() {
    let harness = support::Harness::start();
    let files = harness.files();
    let big: Vec<u8> = (0..2 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
    let chunks = big
        .chunks(1024 * 1024)
        .map(|chunk| files.stage_chunk(chunk).unwrap())
        .collect::<Vec<_>>();
    let changes = serde_json::from_value(serde_json::json!([
        {"put":{"path":"/shared/small.txt","exec":false,"meta":{},"content":{"inline":{"b64":base64::engine::general_purpose::STANDARD.encode(b"small bytes")}}}},
        {"put":{"path":"/shared/big.bin","exec":false,"meta":{},"content":{"chunks":{"size":big.len(),"chunks":chunks}}}}
    ])).unwrap();
    files.commit(None, "file bodies", changes).unwrap();
    let snapshot = files.refs().unwrap().head.unwrap();
    assert_eq!(
        files
            .read("/shared/small.txt", Some(&snapshot), 0, 1024)
            .unwrap(),
        (b"small bytes".to_vec(), true)
    );
    let mut read = Vec::new();
    loop {
        let (page, eof) = files
            .read(
                "/shared/big.bin",
                Some(&snapshot),
                read.len() as u64,
                1024 * 1024,
            )
            .unwrap();
        assert!(!page.is_empty());
        read.extend(page);
        if eof {
            break;
        }
    }
    assert_eq!(read, big);
    files
        .commit(
            Some(&snapshot),
            "remove",
            vec![duckfs_core::Change::Rm {
                path: "/shared/big.bin".into(),
            }],
        )
        .unwrap();
    assert!(files.stat("/shared/big.bin", None).unwrap().is_none());
    // Snapshot-addressed queries still see the immutable prior commit.
    assert_eq!(
        files
            .stat("/shared/big.bin", Some(&snapshot))
            .unwrap()
            .unwrap()
            .size,
        big.len() as u64
    );
}
