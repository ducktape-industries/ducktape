// The node's one writer: the task that owns the node applies each block and serves what callers
// ask of it, in order. RPC handlers reach it through `Writer` and await its answer; nothing else
// holds the node.

use futures::channel::{mpsc, oneshot};

use crate::{Error, Result};

pub(crate) enum Command {
    Sync(Sync),
    Hold(Hold),
    Close(oneshot::Sender<Result<u64>>),
}

pub(crate) struct Sync {
    pub(crate) request: statesync::Request,
    pub(crate) reply: oneshot::Sender<statesync::Served>,
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
pub(crate) struct Writer(mpsc::UnboundedSender<Command>);

impl Writer {
    pub(crate) fn new() -> (Writer, mpsc::UnboundedReceiver<Command>) {
        let (commands, inbox) = mpsc::unbounded();
        (Writer(commands), inbox)
    }

    async fn ask<T>(&self, command: Command, answer: oneshot::Receiver<T>) -> Result<T> {
        self.0.unbounded_send(command).map_err(|_| Error::Stopped)?;
        answer.await.map_err(|_| Error::Stopped)
    }

    pub(crate) async fn sync(&self, request: statesync::Request) -> Result<statesync::Served> {
        let (reply, answer) = oneshot::channel();
        self.ask(Command::Sync(Sync { request, reply }), answer)
            .await
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
