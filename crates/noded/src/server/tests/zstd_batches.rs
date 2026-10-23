use super::*;

use std::sync::atomic::AtomicUsize;

use futures::TryStreamExt as _;

#[tokio::test]
async fn tiny_pieces_share_a_compression_job_and_keep_the_tail() {
    let input_size = zstd::zstd_safe::CCtx::in_size();
    let pieces =
        futures::stream::iter((0..=input_size).map(|_| vec![Ok(Bytes::from_static(&[0]))]));
    let mut batches = compression_batches(pieces).boxed();
    let first = batches.next().await.unwrap().unwrap();
    assert_eq!(first.len(), input_size);
    let tail = batches.next().await.unwrap().unwrap();
    assert_eq!(tail, vec![Bytes::from_static(&[0])]);
    assert!(batches.next().await.is_none());
}

#[tokio::test]
async fn a_batch_keeps_large_pieces_whole_and_stops_reading_at_the_input_goal() {
    let header = Bytes::from_static(&[1]);
    let body = Bytes::from(vec![7; zstd::zstd_safe::CCtx::in_size()]);
    let tail = Bytes::from_static(&[0]);
    let pieces = vec![
        vec![Ok(header.clone()), Ok(body.clone())],
        vec![Ok(tail.clone())],
    ];
    let read = Arc::new(AtomicUsize::new(0));
    let observed = read.clone();
    let pieces = futures::stream::iter(pieces).inspect(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    let mut batches = compression_batches(pieces).boxed();
    assert_eq!(batches.next().await.unwrap().unwrap(), vec![header, body]);
    assert_eq!(read.load(Ordering::SeqCst), 1);
    assert_eq!(batches.next().await.unwrap().unwrap(), vec![tail]);
    assert!(batches.next().await.is_none());
}

#[tokio::test]
async fn compression_reads_only_when_polled_and_keeps_one_batch_ahead() {
    let input_size = zstd::zstd_safe::CCtx::in_size();
    let pieces = vec![
        vec![Ok(Bytes::from(vec![1; input_size]))],
        vec![Ok(Bytes::from(vec![2; input_size]))],
        vec![Ok(Bytes::from(vec![3; input_size]))],
    ];
    let read = Arc::new(AtomicUsize::new(0));
    let observed = read.clone();
    let pieces = futures::stream::iter(pieces).inspect(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    let mut answer = compressed(pieces, Compressor::new().unwrap()).boxed();
    assert_eq!(read.load(Ordering::SeqCst), 0);
    answer.next().await.unwrap().unwrap();
    assert_eq!(read.load(Ordering::SeqCst), 2);
    drop(answer);
    assert_eq!(read.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn blob_records_decode_in_order_across_batches_with_missing_and_repeated_blobs() {
    let big = blob::frame(
        "part",
        &vec![7; zstd::zstd_safe::CCtx::in_size() + zstd::zstd_safe::CCtx::out_size()],
    )
    .unwrap();
    let records = [
        None,
        Some(blob::frame("part", b"").unwrap()),
        Some(big.clone()),
        Some(big),
        None,
    ];
    let count = Bytes::copy_from_slice(&(records.len() as u32).to_le_bytes());
    let mut pieces = vec![vec![Ok(count)]];
    pieces.extend(records.into_iter().map(|record| encoded(Ok(record))));
    let plain = plain_bytes(&pieces);
    let packed = packed(pieces).await;
    assert_eq!(zstd::decode_all(packed.as_slice()).unwrap(), plain);
}

#[tokio::test]
async fn an_empty_stream_and_an_empty_blob_answer_both_finish() {
    let count = Bytes::copy_from_slice(&0u32.to_le_bytes());
    for pieces in [Vec::new(), vec![vec![Ok(count)]]] {
        let plain = plain_bytes(&pieces);
        let packed = packed(pieces).await;
        assert!(!packed.is_empty());
        assert_eq!(zstd::decode_all(packed.as_slice()).unwrap(), plain);
    }
}

#[tokio::test]
async fn a_read_failure_ends_compression_without_reading_or_finishing_past_it() {
    let full = vec![Ok(Bytes::from(vec![7; zstd::zstd_safe::CCtx::in_size()]))];
    for prefix in [Vec::new(), full] {
        let error = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        let failed: Vec<crate::Result<Bytes>> = vec![Err(error.into())];
        let pieces =
            futures::stream::iter([prefix, failed]).chain(futures::stream::poll_fn(|_| {
                panic!("a failed blob read ends the source stream")
            }));
        let mut answer = compressed(pieces, Compressor::new().unwrap()).boxed();
        let error = loop {
            let next = answer.next().await.unwrap();
            if let Err(error) = next {
                break error;
            }
        };
        let crate::Error::Io(error) = error else {
            panic!("the read failure keeps its I/O error");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(answer.next().await.is_none());
    }
}

#[test]
fn one_output_scratch_survives_empty_small_and_full_input_batches() {
    let mut compressor = Compressor::new().unwrap();
    let room = compressor.room.as_ptr();
    let mut packed = Vec::new();
    let mut plain = Vec::new();
    for parts in [
        Vec::new(),
        vec![Bytes::from_static(b"head")],
        vec![Bytes::from(vec![7; zstd::zstd_safe::CCtx::in_size()])],
        vec![Bytes::from_static(b"tail")],
    ] {
        for part in &parts {
            plain.extend_from_slice(part);
        }
        let (next, out) = compressor.compress(&parts).unwrap();
        assert_eq!(next.room.as_ptr(), room);
        assert_eq!(next.room.len(), zstd::zstd_safe::CCtx::out_size());
        packed.extend_from_slice(&out);
        compressor = next;
    }
    packed.extend(compressor.finish().unwrap());
    assert_eq!(zstd::decode_all(packed.as_slice()).unwrap(), plain);
}

fn plain_bytes(pieces: &[Vec<crate::Result<Bytes>>]) -> Vec<u8> {
    pieces
        .iter()
        .flatten()
        .flat_map(|part| part.as_ref().unwrap().iter().copied())
        .collect()
}

async fn packed(pieces: Vec<Vec<crate::Result<Bytes>>>) -> Vec<u8> {
    compressed(futures::stream::iter(pieces), Compressor::new().unwrap())
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .into_iter()
        .flatten()
        .collect()
}
