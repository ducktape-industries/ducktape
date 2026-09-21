use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Registry, reload};

const RING: usize = 4096;

type Filter = reload::Handle<EnvFilter, Registry>;

static LOGS: OnceLock<Logs> = OnceLock::new();

#[derive(Clone)]
pub struct Logs {
    ring: Arc<Mutex<VecDeque<String>>>,
    filter: Filter,
}

impl Logs {
    pub fn install(directives: &str) -> Result<Logs, String> {
        if let Some(logs) = LOGS.get() {
            logs.retune(directives)?;
            return Ok(logs.clone());
        }
        let filter = EnvFilter::try_new(directives).map_err(|error| error.to_string())?;
        let (filter, handle) = reload::Layer::new(filter);
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING)));
        let logs = Logs {
            ring: ring.clone(),
            filter: handle,
        };
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(Ring { ring })
            .try_init()
            .map_err(|error| error.to_string())?;
        Ok(LOGS.get_or_init(|| logs).clone())
    }

    pub fn lines(&self) -> Vec<String> {
        self.ring
            .lock()
            .expect("the log ring lock is never poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub fn retune(&self, directives: &str) -> Result<(), String> {
        let filter = EnvFilter::try_new(directives).map_err(|error| error.to_string())?;
        self.filter
            .reload(filter)
            .map_err(|error| error.to_string())
    }
}

struct Ring {
    ring: Arc<Mutex<VecDeque<String>>>,
}

impl<S> Layer<S> for Ring
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        let mut line = format!("{} {}", metadata.level(), metadata.target());
        event.record(&mut Fields(&mut line));
        let mut ring = self
            .ring
            .lock()
            .expect("the log ring lock is never poisoned");
        if ring.len() == RING {
            ring.pop_front();
        }
        ring.push_back(line);
    }
}

struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let is_the_message = field.name() == "message";
        if is_the_message {
            let _ = write!(self.0, " {value:?}");
            return;
        }
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}
