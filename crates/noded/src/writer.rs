// The node's one writer: the task that owns the node applies each block, in order, and takes the
// few commands that need the node itself. RPC handlers reach it through `Writer`; reads, state sync
// included, are served from what it publishes, and nothing else holds the node.

use commonware_runtime::telemetry::metrics::Gauge;
use futures::channel::{mpsc, oneshot};

use crate::{Error, Result};

pub(crate) enum Command {
    Hold(Hold),
    Close(oneshot::Sender<Result<u64>>),
}

pub(crate) struct Hold {
    pub(crate) holding: oneshot::Sender<()>,
    pub(crate) released: oneshot::Receiver<()>,
}

pub struct Held(oneshot::Sender<()>);

impl Held {
    pub fn release(self) {
        let _ = self.0.send(());
    }
}

#[derive(Clone)]
pub(crate) struct Writer {
    commands: mpsc::UnboundedSender<Command>,
    queued: Gauge,
}

impl Writer {
    pub(crate) fn new(queued: Gauge) -> (Writer, mpsc::UnboundedReceiver<Command>) {
        let (commands, inbox) = mpsc::unbounded();
        (Writer { commands, queued }, inbox)
    }

    async fn ask<T>(&self, command: Command, answer: oneshot::Receiver<T>) -> Result<T> {
        self.queued.inc();
        if self.commands.unbounded_send(command).is_err() {
            self.queued.dec();
            return Err(Error::Stopped);
        }
        answer.await.map_err(|_| Error::Stopped)
    }

    pub(crate) async fn hold(&self) -> Result<Held> {
        let (holding, held) = oneshot::channel();
        let (release, released) = oneshot::channel();
        self.ask(Command::Hold(Hold { holding, released }), held)
            .await?;
        Ok(Held(release))
    }

    pub(crate) async fn close(&self) -> Result<u64> {
        let (reply, answer) = oneshot::channel();
        self.ask(Command::Close(reply), answer).await?
    }
}
