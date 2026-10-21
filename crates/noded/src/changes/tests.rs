use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use futures::{FutureExt as _, StreamExt as _};

use super::*;

type E = commonware_runtime::tokio::Context;

fn network() -> Network {
    Network {
        epoch_length: 4,
        cadence: consensus::Cadence::new(Duration::from_secs(1), consensus::Timeouts::default())
            .unwrap(),
    }
}

fn head(height: u64) -> Head {
    Head {
        height,
        id: [height as u8; 32],
        time: 1_000 * height,
    }
}

fn wrote(height: u64, program: Option<&str>) -> Wrote<E> {
    let mut writes = Writes::default();
    if let Some(program) = program {
        let slots = BTreeMap::from([(b"key".to_vec(), Some(b"value".to_vec()))]);
        writes.programs.insert(program.to_owned(), slots);
    }
    Wrote {
        head: head(height),
        root: abi::Root([0; 32]),
        writes: Arc::new(writes),
        places: Arc::default(),
        view: None,
    }
}

fn step(fanout: &mut Fanout<E>) {
    while let Ok(event) = fanout.inbox.try_recv() {
        match event {
            Event::Subscribe(subscriber) => subscribed(fanout, subscriber),
            Event::Wrote(wrote) => written(fanout, wrote),
            Event::Take(take) => taken(fanout, take),
            Event::HangUp(_) | Event::Unasked => Flow::Closed,
            Event::Opened(head) => opened(fanout, head),
        };
    }
}

fn heard(subscription: &mut Subscription) -> Vec<String> {
    let mut words = Vec::new();
    while let Some(Some(word)) = subscription.changes.next().now_or_never() {
        words.push(match word {
            Published::Head(head) => {
                assert_eq!(head, self::head(head.height), "a head as the block had it");
                format!("head {}", head.height)
            }
            Published::Change(change) => {
                let keys = change.writes.iter().all(|(_, value)| value.is_none());
                format!("change {} keys {keys}", change.height)
            }
            Published::Ping(height) => format!("ping {height}"),
        });
    }
    words
}

#[test]
fn a_heads_socket_on_a_node_that_applied_nothing_since_it_started_opens_on_its_tip() {
    let (changes, mut fanout) = Changes::<E>::new(network());
    changes.opened(head(7));
    let mut heads = changes.subscribe(Follows::Heads);
    step(&mut fanout);
    assert_eq!(heard(&mut heads), ["head 7"]);
    changes.wrote(wrote(8, None));
    changes.opened(head(7));
    let mut later = changes.subscribe(Follows::Heads);
    step(&mut fanout);
    assert_eq!(heard(&mut heads), ["head 8"]);
    assert_eq!(heard(&mut later), ["head 8"]);
}

#[test]
fn a_heads_socket_opens_on_the_newest_head_and_hears_every_block_after() {
    let (changes, mut fanout) = Changes::<E>::new(network());
    changes.wrote(wrote(1, None));
    step(&mut fanout);
    let mut heads = changes.subscribe(Follows::Heads);
    step(&mut fanout);
    for height in 2..=4 {
        changes.wrote(wrote(height, Some("chat")));
    }
    step(&mut fanout);
    assert_eq!(
        heard(&mut heads),
        ["head 1", "head 2", "head 3", "ping 3", "head 4"]
    );
}

#[test]
fn a_program_socket_hears_its_writes_and_a_ping_for_each_other_block() {
    let (changes, mut fanout) = Changes::<E>::new(network());
    let program = |keys| Follows::Program {
        program: "chat".to_owned(),
        keys,
    };
    let (mut keys, mut values) = (
        changes.subscribe(program(true)),
        changes.subscribe(program(false)),
    );
    let mut heads = changes.subscribe(Follows::Heads);
    step(&mut fanout);
    changes.wrote(wrote(1, Some("chat")));
    changes.wrote(wrote(2, None));
    changes.wrote(wrote(3, Some("chat")));
    step(&mut fanout);
    assert_eq!(
        heard(&mut keys),
        [
            "change 1 keys true",
            "ping 2",
            "change 3 keys true",
            "ping 3"
        ]
    );
    assert_eq!(
        heard(&mut values),
        [
            "change 1 keys false",
            "ping 2",
            "change 3 keys false",
            "ping 3"
        ]
    );
    assert_eq!(heard(&mut heads), ["head 1", "head 2", "head 3", "ping 3"]);
}

#[test]
fn a_program_socket_hears_its_place_under_its_new_id_and_the_old_id_hears_pings() {
    let (changes, mut fanout) = Changes::<E>::new(network());
    let program = |program: &str| Follows::Program {
        program: program.to_owned(),
        keys: false,
    };
    let (mut renamed, mut old) = (
        changes.subscribe(program("owner/chat")),
        changes.subscribe(program("chat")),
    );
    step(&mut fanout);
    let places = host::Places::new(BTreeMap::from([(
        "owner/chat".to_owned(),
        "chat".to_owned(),
    )]));
    changes.wrote(Wrote {
        places: Arc::new(places),
        ..wrote(1, Some("chat"))
    });
    step(&mut fanout);
    assert_eq!(heard(&mut renamed), ["change 1 keys false"]);
    assert_eq!(heard(&mut old), ["ping 1"]);
}

#[test]
fn a_socket_still_writing_a_block_from_before_the_last_close_is_cut_and_one_writing_a_newer_stays()
{
    let (changes, mut fanout) = Changes::<E>::new(network());
    let mut stalled = changes.subscribe(Follows::Heads);
    let mut slow = changes.subscribe(Follows::Heads);
    let mut drained = changes.subscribe(Follows::Heads);
    step(&mut fanout);
    for height in 1..=3 {
        changes.wrote(wrote(height, None));
    }
    step(&mut fanout);
    stalled.writing.begin(2);
    slow.writing.begin(3);
    for height in 4..=7 {
        changes.wrote(wrote(height, None));
    }
    step(&mut fanout);
    assert_eq!(heard(&mut slow).last().map(String::as_str), Some("ping 7"));
    assert_eq!(
        heard(&mut drained).last().map(String::as_str),
        Some("ping 7")
    );
    assert_eq!(
        heard(&mut stalled).last().map(String::as_str),
        Some("head 6")
    );
    assert!(
        matches!(stalled.changes.next().now_or_never(), Some(None)),
        "the stalled socket was cut"
    );
}

fn wrote_keys(height: u64, writes: &[(&str, &str)]) -> Wrote<E> {
    let mut programs: BTreeMap<String, BTreeMap<Vec<u8>, Option<Vec<u8>>>> = BTreeMap::new();
    for (program, key) in writes {
        let value = Some(height.to_le_bytes().to_vec());
        programs
            .entry(program.to_string())
            .or_default()
            .insert(key.as_bytes().to_vec(), value);
    }
    Wrote {
        head: head(height),
        root: abi::Root([0; 32]),
        writes: Arc::new(Writes {
            programs,
            local: Default::default(),
        }),
        places: Arc::default(),
        view: None,
    }
}

#[test]
fn a_watch_socket_that_pulls_late_gets_every_block_since_merged_into_one_then_waits_for_the_next() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let cadence =
        consensus::Cadence::new(Duration::from_millis(250), consensus::Timeouts::default())
            .unwrap();
    let network = Network {
        epoch_length: 1_000,
        cadence,
    };
    let (changes, fanout) = Changes::<E>::new(network);
    runtime.spawn(fan(fanout));
    runtime.block_on(async {
        let pull = changes.pull("probe".into(), false);
        changes.wrote(wrote_keys(1, &[("probe", "a"), ("other", "x")]));
        changes.wrote(wrote_keys(2, &[("probe", "a"), ("probe", "b")]));
        let first = pull.next();
        let second = pull.next();
        changes.wrote(wrote_keys(3, &[("other", "y")]));
        changes.hang_up().await;
        let merged = first
            .await
            .expect("a pull with blocks waiting is answered at once");
        assert_eq!(merged.height, 2);
        let newest = Some(2u64.to_le_bytes().to_vec());
        let writes = BTreeMap::from([(b"a".to_vec(), newest.clone()), (b"b".to_vec(), newest)]);
        assert_eq!(merged.writes, writes);
        let written = host::Written::from([
            ("other".to_owned(), BTreeSet::from([b"x".to_vec()])),
            (
                "probe".to_owned(),
                BTreeSet::from([b"a".to_vec(), b"b".to_vec()]),
            ),
        ]);
        assert_eq!(merged.written, written);
        let merged = second
            .await
            .expect("a pull with no block waiting is answered by the next one");
        assert_eq!(merged.height, 3);
        assert!(merged.writes.is_empty());
    });
}
