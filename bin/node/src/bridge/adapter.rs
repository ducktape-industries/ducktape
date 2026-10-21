//! What the bridge asks of an agent product: a session started in a place or resumed by its id,
//! input said to it while it runs, the end of its input, and a stop, with its output as events.
use futures::stream::LocalBoxStream;

use super::kept::Where;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Session(String),
    Text(String),
    Doing(String),
    Answer(String),
    Ended(String),
}

pub(crate) trait Adapter {
    type Turn: Turn;

    fn start(
        &self,
        at: &Where,
        resume: Option<&str>,
    ) -> Result<(Self::Turn, LocalBoxStream<'static, Event>), String>;
}

pub(crate) trait Turn {
    async fn say(&mut self, text: &str) -> Result<(), String>;
    fn finish(&mut self);
    fn stop(&mut self);
}
