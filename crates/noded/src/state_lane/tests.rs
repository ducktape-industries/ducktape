//! The raw-state adapter moves ops over real QUIC and ends holds on stream closure.
use super::*;
use commonware_codec::Decode as _;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::PrivateKey;
use commonware_runtime::{Runner as _, Supervisor as _, tokio as runtime};
use halo::{Config, Lane, Queues, Seat};
use std::collections::BTreeMap;

struct Peer {
    halo: Arc<Halo>,
    seat: Seat,
}

fn peer(context: &runtime::Context, seed: u64) -> Peer {
    let key = PrivateKey::from_seed(seed);
    let config = Config {
        egress_bytes_per_sec: None,
        udp: std::net::UdpSocket::bind("127.0.0.1:0").unwrap().into(),
        queues: Queues::default(),
        reach: halo::Https::shared(),
    };
    let halo = Arc::new(Halo::start(context, key.clone(), config).unwrap());
    halo.plane().lanes(vec![Lane {
        name: statesync::lane::LANE.into(),
        id: 0,
        cap: Class::Member,
        enabled: true,
        pinned: false,
        since: 0,
    }]);
    let seat = Seat {
        key: key.public_key(),
        halo: Some(format!("localhost:{}", halo.udp().unwrap().port())),
        https: None,
        validator: true,
    };
    Peer { halo, seat }
}

fn pair(context: &runtime::Context) -> (Peer, Source, Arc<Streams<Arc<Links>>>) {
    let server = peer(&context.child("server"), 1);
    let client = peer(&context.child("client"), 2);
    let seats = vec![server.seat.clone(), client.seat.clone()];
    server.halo.seat(seats.clone());
    client.halo.seat(seats);
    let inbound = Arc::new(
        server
            .halo
            .plane()
            .streams(statesync::lane::LANE, 0)
            .unwrap(),
    );
    let source = Source::new(client.halo, server.seat.key.clone()).unwrap();
    (server, source, inbound)
}

#[test]
fn raw_ops_cross_quic_in_one_request_and_resume_at_the_next_operation() {
    runtime::Runner::default().start(|context| async move {
        let (_server, source, inbound) = pair(&context);
        let mut commitment = state::Commitment::open(context.child("state"), "served", 0)
            .await
            .unwrap();
        let writes = (0..24u64)
            .map(|key| (key.to_le_bytes().to_vec(), Some(key.to_le_bytes().to_vec())))
            .collect::<BTreeMap<_, _>>();
        commitment.apply(1, &writes).await.unwrap();
        let serving = commitment.serving();
        let target = serving.target.clone().unwrap();
        let start = *target.range.start();
        let first = serving.ops(start, NonZeroU64::MIN).await.unwrap();
        let resumed = start + first.len() as u64;
        let count = NonZeroU64::new(*target.range.end() - start).unwrap();
        let expected = serving.ops(start, count).await.unwrap().encode();
        let serving_task = tokio::spawn(async move {
            let mut cut = inbound.accept().await.unwrap();
            let ask: Ask = borsh::from_slice(&cut.meta).unwrap();
            assert_eq!(
                ask,
                Ask::Ops {
                    hold: 9,
                    program: "p".into(),
                    from: start
                }
            );
            write(
                &mut cut.stream,
                &Ok(Piece::Pinned(
                    serving
                        .pinned()
                        .await
                        .unwrap()
                        .into_iter()
                        .map(|d| d.0)
                        .collect(),
                )),
            )
            .await
            .unwrap();
            write(&mut cut.stream, &Ok(Piece::Ops(first.encode().to_vec())))
                .await
                .unwrap();
            cut.stream.shutdown().await.unwrap();
            drop(cut);
            let mut next = inbound.accept().await.unwrap();
            let ask: Ask = borsh::from_slice(&next.meta).unwrap();
            assert_eq!(
                ask,
                Ask::Ops {
                    hold: 9,
                    program: "p".into(),
                    from: resumed
                }
            );
            serve_ops(&serving, resumed, NonZeroU64::MIN, &mut next.stream)
                .await
                .unwrap();
        });
        let mut received = Vec::<state::Op>::new();
        let mut first = std::pin::pin!(source.ops(9, "p".into(), start));
        assert!(matches!(
            first.next().await.unwrap().unwrap(),
            Piece::Pinned(_)
        ));
        let Piece::Ops(ops) = first.next().await.unwrap().unwrap() else {
            panic!("missing first ops");
        };
        received.extend(
            Vec::<state::Op>::decode_cfg(
                ops.as_slice(),
                &(commonware_codec::RangeCfg::from(..), state::codec_config()),
            )
            .unwrap(),
        );
        let cut = first.next().await.unwrap().unwrap_err();
        assert!(matches!(cut.failure(), Failure::Transient(_)));
        assert!(first.next().await.is_none());
        let mut next = std::pin::pin!(source.ops(9, "p".into(), resumed));
        assert!(matches!(
            next.next().await.unwrap().unwrap(),
            Piece::Pinned(_)
        ));
        loop {
            match next.next().await.unwrap().unwrap() {
                Piece::Ops(ops) => received.extend(
                    Vec::<state::Op>::decode_cfg(
                        ops.as_slice(),
                        &(commonware_codec::RangeCfg::from(..), state::codec_config()),
                    )
                    .unwrap(),
                ),
                Piece::End => break,
                Piece::Pinned(_) => panic!("duplicate pinned nodes"),
            }
        }
        assert!(next.next().await.is_none());
        serving_task.await.unwrap();
        assert!(
            received.encode() == expected,
            "raw state operations were lost during transfer"
        );
    });
}

#[test]
fn a_source_refusal_is_not_a_transient_cut() {
    runtime::Runner::default().start(|context| async move {
        let (_server, source, inbound) = pair(&context);
        let task = tokio::spawn(async move {
            let mut inbound = inbound.accept().await.unwrap();
            write::<_, Piece>(
                &mut inbound.stream,
                &Err(Refusal::new(reason::NOT_FOUND, "no such hold")),
            )
            .await
            .unwrap();
        });
        let mut pieces = std::pin::pin!(source.ops(9, "p".into(), 0));
        let error = pieces.next().await.unwrap().unwrap_err();
        assert!(matches!(error, Error::Refused(_)));
        assert_eq!(error.failure(), Failure::Definite);
        assert!(pieces.next().await.is_none());
        task.await.unwrap();
    });
}

#[test]
fn the_hold_ends_on_the_quic_streams_end_not_a_clock() {
    runtime::Runner::default().start(|context| async move {
        let (_server, source, inbound) = pair(&context);
        let (close, closed) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut inbound = inbound.accept().await.unwrap();
            assert_eq!(
                borsh::from_slice::<Ask>(&inbound.meta).unwrap(),
                Ask::Head { past: None }
            );
            closed.await.unwrap();
            inbound.stream.shutdown().await.unwrap();
        });
        let stream = source.open(Ask::Head { past: None }).await.unwrap();
        let mut ending = until_cut(source.halo.clone(), stream);
        assert!(futures::poll!(&mut ending).is_pending());
        close.send(()).unwrap();
        ending.await;
        task.await.unwrap();
    });
}

#[test]
fn a_blob_request_larger_than_hello_metadata_still_uses_one_stream() {
    runtime::Runner::default().start(|context| async move {
        let (_server, source, inbound) = pair(&context);
        let id = BlobId::Sha256([7; 32]);
        let count = usize::from(u16::MAX) / abi::encode(&id).len() + 1;
        let ids = vec![id; count];
        assert!(abi::encode(&ids).len() > usize::from(u16::MAX));
        let expected = ids.clone();
        let task = tokio::spawn(async move {
            let mut inbound = inbound.accept().await.unwrap();
            assert_eq!(borsh::from_slice::<Ask>(&inbound.meta).unwrap(), Ask::Blobs);
            let requested: Vec<BlobId> = read(&mut inbound.stream).await.unwrap();
            assert!(requested == expected);
            write(&mut inbound.stream, &Ok(None::<Vec<u8>>))
                .await
                .unwrap();
            inbound.stream.shutdown().await.unwrap();
        });
        let mut blobs = std::pin::pin!(source.stream_blobs(ids));
        assert_eq!(blobs.next().await.unwrap().unwrap(), None);
        task.await.unwrap();
    });
}
