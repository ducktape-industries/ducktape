use std::net::SocketAddr;
use std::time::Duration;

use commonware_runtime::mocks::Channel;
use commonware_runtime::{
    Clock as _, Error, IoBufs, Listener as _, Network as _, Runner as _, Sink as _, Spawner as _,
    Stream as _, Supervisor as _, deterministic,
};
use consensus::{Cadence, Timeouts};

use super::{Linked, Sink, Stream};
use crate::mesh::{link_timeout, ping_frequency};

const IDLE: Duration = Duration::from_secs(20);

fn message(len: usize) -> Vec<u8> {
    (0..len).map(|at| at as u8).collect()
}

fn seconds_since(context: &deterministic::Context, start: std::time::SystemTime) -> u64 {
    context.current().duration_since(start).unwrap().as_secs()
}

#[test]
fn a_message_that_keeps_arriving_is_received_however_long_it_takes() {
    deterministic::Runner::default().start(|context| async move {
        let (mut sender, receiver) = Channel::init();
        let mut stream = Stream {
            inner: receiver,
            clock: context.child("link"),
            idle: IDLE,
        };
        let sent = message(64 << 10);
        let trickle = sent.clone();
        let _sending = context.child("sender").spawn(move |context| async move {
            for piece in trickle.chunks(1 << 10) {
                sender.send(IoBufs::from(piece.to_vec())).await.unwrap();
                context.sleep(Duration::from_secs(1)).await;
            }
            sender
        });
        let start = context.current();

        let received = stream.recv(sent.len()).await.unwrap();

        assert_eq!(received.coalesce().as_ref(), sent.as_slice());
        assert!(seconds_since(&context, start) >= 63);
    });
}

#[test]
fn a_link_that_falls_silent_mid_message_dies_after_the_idle_limit() {
    deterministic::Runner::default().start(|context| async move {
        let (mut sender, receiver) = Channel::init();
        let mut stream = Stream {
            inner: receiver,
            clock: context.child("link"),
            idle: IDLE,
        };
        sender.send(IoBufs::from(message(1 << 10))).await.unwrap();
        let start = context.current();

        let refused = stream.recv(64 << 10).await;

        assert!(matches!(refused, Err(Error::Timeout)), "{refused:?}");
        assert_eq!(seconds_since(&context, start), IDLE.as_secs());
        drop(sender);
    });
}

#[test]
fn a_send_to_a_peer_that_keeps_reading_finishes_however_long_it_takes() {
    deterministic::Runner::default().start(|context| async move {
        let (sender, mut receiver) = Channel::init_with_buffer_size(1 << 10);
        let mut sink = Sink {
            inner: sender,
            clock: context.child("link"),
            idle: IDLE,
        };
        let sent = message(1 << 20);
        let len = sent.len();
        let reader = context.child("reader").spawn(move |context| async move {
            let mut received = Vec::new();
            while received.len() < len {
                let piece = (len - received.len()).min(8 << 10);
                received.extend_from_slice(receiver.recv(piece).await.unwrap().coalesce().as_ref());
                context.sleep(Duration::from_secs(1)).await;
            }
            received
        });
        let start = context.current();

        sink.send(IoBufs::from(sent.clone())).await.unwrap();

        assert!(seconds_since(&context, start) >= 120);
        assert_eq!(reader.await.unwrap(), sent);
    });
}

#[test]
fn a_send_to_a_peer_that_stops_reading_dies_after_the_idle_limit() {
    deterministic::Runner::default().start(|context| async move {
        let (sender, receiver) = Channel::init_with_buffer_size(1 << 10);
        let mut sink = Sink {
            inner: sender,
            clock: context.child("link"),
            idle: IDLE,
        };
        let start = context.current();

        let refused = sink.send(IoBufs::from(message(1 << 20))).await;

        assert!(matches!(refused, Err(Error::Timeout)), "{refused:?}");
        assert_eq!(seconds_since(&context, start), IDLE.as_secs());
        drop(receiver);
    });
}

fn five_second_blocks() -> Cadence {
    Cadence::new(Duration::from_secs(5), Timeouts::default()).unwrap()
}

#[test]
fn at_five_second_blocks_a_silent_link_dies_after_twenty_block_times() {
    deterministic::Runner::default().start(|context| async move {
        let cadence = five_second_blocks();
        let linked = Linked::new(context.child("linked"), link_timeout(cadence));
        let address = SocketAddr::from(([127, 0, 0, 1], 9000));
        let mut listener = linked.bind(address).await.unwrap();
        let (dialed, _) = linked.dial(address).await.unwrap();
        let (_, _, mut stream) = listener.accept().await.unwrap();
        let start = context.current();

        let refused = stream.recv(1).await;

        assert!(matches!(refused, Err(Error::Timeout)), "{refused:?}");
        assert_eq!(seconds_since(&context, start), 100);
        drop(dialed);
    });
}

#[test]
fn at_five_second_blocks_an_idle_link_pinged_every_five_block_times_stays_up() {
    deterministic::Runner::default().start(|context| async move {
        let cadence = five_second_blocks();
        let linked = Linked::new(context.child("linked"), link_timeout(cadence));
        let address = SocketAddr::from(([127, 0, 0, 1], 9000));
        let mut listener = linked.bind(address).await.unwrap();
        let (mut pinger, _) = linked.dial(address).await.unwrap();
        let (_, _, mut stream) = listener.accept().await.unwrap();
        let pings = 12;
        let _pinging = context.child("pinger").spawn(move |context| async move {
            for _ in 0..pings {
                context.sleep(ping_frequency(cadence)).await;
                pinger.send(IoBufs::from(vec![0u8])).await.unwrap();
            }
            pinger
        });
        let start = context.current();

        for _ in 0..pings {
            stream.recv(1).await.unwrap();
        }

        assert_eq!(seconds_since(&context, start), 300);
    });
}
