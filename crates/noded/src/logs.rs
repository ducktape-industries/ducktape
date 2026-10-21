use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwapOption;

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Registry, reload};

pub(crate) const RING: usize = 4096;

type Filter = reload::Handle<EnvFilter, Registry>;

static LOGS: OnceLock<Result<Logs, String>> = OnceLock::new();

#[derive(Clone)]
pub struct Logs {
    ring: Arc<Lines>,
    filter: Filter,
}

struct Lines {
    slots: Box<[ArcSwapOption<Line>]>,
    next: AtomicU64,
}

struct Line {
    at: u64,
    text: String,
}

impl Logs {
    pub fn install(directives: &str) -> Result<Logs, String> {
        let logs = LOGS.get_or_init(|| Logs::installed(directives)).clone()?;
        logs.retune(directives)?;
        Ok(logs)
    }

    fn installed(directives: &str) -> Result<Logs, String> {
        let filter = EnvFilter::try_new(directives).map_err(|error| error.to_string())?;
        let (filter, handle) = reload::Layer::new(filter);
        let ring = Arc::new(Lines {
            slots: (0..RING).map(|_| ArcSwapOption::empty()).collect(),
            next: AtomicU64::new(0),
        });
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
        Ok(logs)
    }

    pub fn lines(&self) -> Vec<String> {
        let end = self.ring.next.load(Ordering::Acquire);
        let start = end.saturating_sub(RING as u64);
        let mut kept: Vec<Arc<Line>> = self
            .ring
            .slots
            .iter()
            .filter_map(ArcSwapOption::load_full)
            .filter(|line| (start..end).contains(&line.at))
            .collect();
        kept.sort_unstable_by_key(|line| line.at);
        kept.iter().map(|line| line.text.clone()).collect()
    }

    pub fn retune(&self, directives: &str) -> Result<(), String> {
        let filter = EnvFilter::try_new(directives).map_err(|error| error.to_string())?;
        self.filter
            .reload(filter)
            .map_err(|error| error.to_string())
    }
}

struct Ring {
    ring: Arc<Lines>,
}

impl<S> Layer<S> for Ring
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        let mut line = format!("{} {}", metadata.level(), metadata.target());
        event.record(&mut Fields(&mut line));
        let at = self.ring.next.fetch_add(1, Ordering::AcqRel);
        let slot = &self.ring.slots[(at % RING as u64) as usize];
        slot.store(Some(Arc::new(Line { at, text: line })));
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
