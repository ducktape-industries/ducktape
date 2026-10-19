//! A store opens every program's commitment at once, each on its own thread and with a cache sized
//! to the keys storage holds for it: every commitment comes back at the root a one-by-one open
//! without a cache gives it, and a failed open fails the store's open.
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::Path;
use std::sync::{Arc, Mutex};

use commonware_runtime::{Runner as _, Supervisor as _, deterministic, tokio};
use commonware_utils::probability;
use state::{Commitment, Storage, Store, Writes, commitment_name};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

const PROGRAMS: u64 = 12;
const BLOCKS: u64 = 24;

fn programs() -> Vec<String> {
    (0..PROGRAMS)
        .map(|index| format!("program{index}"))
        .collect()
}

fn block(height: u64) -> Writes {
    let keys = |index: u64| {
        (0..8 * (index + 1))
            .map(|key| {
                let name = format!("{index}/{key}").into_bytes();
                let deleted = (key + height).is_multiple_of(7);
                let value = format!("{height}/{key}").into_bytes();
                (name, (!deleted).then_some(value))
            })
            .collect::<BTreeMap<_, _>>()
    };
    Writes {
        programs: programs()
            .into_iter()
            .zip(0..)
            .map(|(program, index)| (program, keys(index)))
            .collect(),
    }
}

fn storage(dir: &Path) -> Storage {
    Storage::open(&dir.join("state")).unwrap()
}

fn runner(dir: &Path) -> tokio::Runner {
    tokio::Runner::new(tokio::Config::new().with_storage_directory(dir.join("runtime")))
}

#[test]
fn every_commitment_a_store_opens_at_once_has_the_root_a_one_by_one_open_gives_it() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let committed = runner(dir).start(|context| async move {
        let mut store = Store::open(context.child("store"), "s", storage(dir), programs())
            .await
            .unwrap();
        for height in 0..BLOCKS {
            store.commit(height, block(height)).await.unwrap();
        }
        store.sync().await.unwrap();
        BTreeMap::from_iter(store.roots().unwrap())
    });
    let (empty, one_by_one) = runner(dir).start(|context| async move {
        let empty = Commitment::open(context.child("empty"), "empty", 0)
            .await
            .unwrap()
            .root()
            .unwrap();
        let mut roots = BTreeMap::new();
        for program in programs() {
            let context = context
                .child("one_by_one")
                .with_attribute("program", &program);
            let commitment = Commitment::open(context, &commitment_name("s", &program), 0)
                .await
                .unwrap();
            roots.insert(program, commitment.root().unwrap());
        }
        (empty, roots)
    });
    let at_once = runner(dir).start(|context| async move {
        let store = Store::open(context.child("store"), "s", storage(dir), programs())
            .await
            .unwrap();
        assert_eq!(store.height().unwrap(), Some(BLOCKS - 1));
        BTreeMap::from_iter(store.roots().unwrap())
    });
    assert!(one_by_one.values().all(|root| *root != empty));
    assert_eq!(at_once, one_by_one);
    assert_eq!(at_once, committed);
}

#[derive(Clone, Default)]
struct Opened(Arc<Mutex<BTreeMap<String, u64>>>);

#[derive(Default)]
struct Fields {
    program: Option<String>,
    live_keys: Option<u64>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "program" {
            self.program = Some(value.to_owned());
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "live_keys" {
            self.live_keys = Some(value);
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
}

impl<S: Subscriber> Layer<S> for Opened {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let (Some(program), Some(live_keys)) = (fields.program, fields.live_keys) else {
            return;
        };
        self.0.lock().unwrap().insert(program, live_keys);
    }
}

#[test]
fn a_store_sizes_each_open_to_the_keys_storage_holds_for_its_program() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let (held, checkpoint) =
        deterministic::Runner::default().start_and_recover(|context| async move {
            let mut store = Store::open(context.child("store"), "s", storage(dir), programs())
                .await
                .unwrap();
            for height in 0..BLOCKS {
                store.commit(height, block(height)).await.unwrap();
            }
            store.sync().await.unwrap();
            programs()
                .into_iter()
                .map(|program| {
                    let keys = store.storage().iter(&program, b"", None, false).unwrap();
                    let count = u64::try_from(keys.count()).unwrap();
                    (program, count)
                })
                .collect::<BTreeMap<_, _>>()
        });
    let opened = Opened::default();
    let subscriber = tracing_subscriber::registry().with(opened.clone());
    tracing::subscriber::with_default(subscriber, || {
        deterministic::Runner::from(checkpoint).start(|context| async move {
            Store::open(context.child("reopened"), "s", storage(dir), programs())
                .await
                .unwrap();
        });
    });
    assert!(held.values().all(|count| *count > 0));
    assert_eq!(*opened.0.lock().unwrap(), held);
}

#[test]
fn a_store_whose_commitments_cannot_open_fails_its_open() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let ((), checkpoint) =
        deterministic::Runner::default().start_and_recover(|context| async move {
            let mut store = Store::open(context.child("store"), "s", storage(dir), programs())
                .await
                .unwrap();
            for height in 0..BLOCKS {
                store.commit(height, block(height)).await.unwrap();
            }
            store.sync().await.unwrap();
        });
    deterministic::Runner::from(checkpoint).start(|context| async move {
        let faults = context.storage_fault_config();
        *faults.write() = deterministic::FaultConfig::default().open(probability!(1.0));
        let refused = Store::open(context.child("reopened"), "s", storage(dir), programs())
            .await
            .err();
        assert!(
            matches!(refused, Some(state::Error::Commitment(_))),
            "{refused:?}"
        );
    });
}
