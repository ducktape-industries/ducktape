//! A store sizes each commitment's cache to the keys storage holds for its program. Its own binary:
//! tracing caches a callsite's interest per process, set by whichever thread registers it first,
//! so a test that captures events through a scoped subscriber shares the process with no thread.
mod common;

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use common::{BLOCKS, block, programs, storage};
use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use state::Store;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

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
