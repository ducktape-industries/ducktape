//! A store synced before its process stops opens again with nothing to replay; one that stops
//! unsynced replays its write log into memory and catches its merkle up to the log when it opens.
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use state::{Storage, Store, Writes};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

const APP: &str = "app";
const OTHER: &str = "other";
const BLOCKS: u64 = 40;

#[derive(Clone, Copy)]
enum Stopped {
    Synced,
    Unsynced,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Opened {
    replayed: Option<u64>,
    recovered: usize,
}

#[derive(Clone, Default)]
struct Watch(Arc<Mutex<Opened>>);

#[derive(Default)]
struct Replayed(Option<u64>);

impl Visit for Replayed {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "records_replayed" {
            self.0 = Some(value);
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "records_replayed" {
            self.0 = u64::try_from(value).ok();
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
}

impl<S: Subscriber> Layer<S> for Watch {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        let mut opened = self.0.lock().unwrap();
        let by_storage = metadata.target().starts_with("commonware_storage");
        if by_storage && *metadata.level() == Level::WARN {
            opened.recovered += 1;
        }
        let mut replayed = Replayed::default();
        event.record(&mut replayed);
        if let Some(records) = replayed.0 {
            opened.replayed = Some(records);
        }
    }
}

fn block(height: u64) -> Writes {
    let keys = |program: &str| {
        (0..8u64)
            .map(|key| {
                let name = format!("{program}/{key}").into_bytes();
                let value = format!("{height}/{key}").into_bytes();
                (name, Some(value))
            })
            .collect::<BTreeMap<_, _>>()
    };
    Writes {
        programs: BTreeMap::from([(APP.to_owned(), keys(APP)), (OTHER.to_owned(), keys(OTHER))]),
    }
}

fn programs() -> [String; 2] {
    [APP.to_owned(), OTHER.to_owned()]
}

fn reopened(stopped: Stopped) -> Opened {
    let dir = tempfile::tempdir().unwrap();
    let (roots, checkpoint) = deterministic::Runner::default().start_and_recover(|context| {
        let dir = &dir;
        async move {
            let storage = Storage::open(dir.path()).unwrap();
            let mut store = Store::open(context.child("store"), "s", storage, programs())
                .await
                .unwrap();
            for height in 0..BLOCKS {
                store.commit(height, block(height)).await.unwrap();
            }
            if let Stopped::Synced = stopped {
                store.sync().await.unwrap();
            }
            store.roots().unwrap()
        }
    });
    let watch = Watch::default();
    let subscriber = tracing_subscriber::registry().with(watch.clone());
    tracing::subscriber::with_default(subscriber, || {
        deterministic::Runner::from(checkpoint).start(|context| async move {
            let storage = Storage::open(dir.path()).unwrap();
            let store = Store::open(context.child("reopened"), "s", storage, programs())
                .await
                .unwrap();
            assert_eq!(store.height().unwrap(), Some(BLOCKS - 1));
            assert_eq!(store.roots().unwrap(), roots);
        });
    });
    std::mem::take(&mut *watch.0.lock().unwrap())
}

#[test]
fn a_store_synced_before_it_stops_opens_with_nothing_to_replay() {
    let opened = reopened(Stopped::Synced);
    assert_eq!(
        opened,
        Opened {
            replayed: Some(0),
            recovered: 0
        }
    );
}

#[test]
fn a_store_stopped_unsynced_replays_its_write_log_and_reopens_at_its_roots() {
    let opened = reopened(Stopped::Unsynced);
    assert!(
        opened.replayed.is_some_and(|records| records > 0),
        "{opened:?}"
    );
}
