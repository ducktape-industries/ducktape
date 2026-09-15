//! Bounded service requests and executor events, with independent spawn jobs.
use crate::state::{Caller, Effect, Mode, Replay, Sessions, Write};
use agent_service::wire;
use base64::Engine as _;
use std::{collections::BTreeMap, path::PathBuf};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
};

const LANE: usize = 64;
const MAX_INPUT_BYTES: usize = 64 * 1024;
type Reply = oneshot::Sender<Result<(), String>>;
type CreateReply = oneshot::Sender<Result<oneshot::Sender<()>, String>>;

#[derive(Clone)]
pub struct Runtime {
    requests: mpsc::Sender<Request>,
    changes: watch::Receiver<()>,
}

enum Request {
    Create {
        caller: Caller,
        mode: Mode,
        spec: wire::Create,
        reply: CreateReply,
    },
    Drive {
        session: String,
        caller: Caller,
        kind: Write,
        command: wire::Command,
        reply: Reply,
    },
    Replay {
        session: String,
        caller: Caller,
        after: u64,
        reply: oneshot::Sender<Result<Replay, String>>,
    },
    Stop,
}

enum Action {
    Create(wire::Create),
    Drive {
        command: wire::Command,
        reply: Reply,
    },
    Close {
        session: String,
        reply: Option<Reply>,
    },
    Reply(Reply, Result<(), String>),
    Created {
        session: String,
        reply: CreateReply,
    },
    CreateFailed(CreateReply, String),
    Replay(
        oneshot::Sender<Result<Replay, String>>,
        Result<Replay, String>,
    ),
    Changed,
    Stop,
}

#[derive(Default)]
struct Machine {
    sessions: Sessions,
    pending: BTreeMap<String, CreateReply>,
}

impl Machine {
    fn request(&mut self, request: Request, workers: usize) -> Vec<Action> {
        match request {
            Request::Create {
                caller,
                mode,
                spec,
                reply,
            } => self.create(caller, mode, spec, reply, workers),
            Request::Drive {
                session,
                caller,
                kind,
                command,
                reply,
            } => self.drive(session, caller, kind, command, reply),
            Request::Replay {
                session,
                caller,
                after,
                reply,
            } => self.replay(session, caller, after, reply),
            Request::Stop => Self::stop(),
        }
    }

    fn create(
        &mut self,
        caller: Caller,
        mode: Mode,
        spec: wire::Create,
        reply: CreateReply,
        workers: usize,
    ) -> Vec<Action> {
        if reply.is_closed() {
            return Vec::new();
        }
        if workers >= LANE {
            return vec![Action::CreateFailed(
                reply,
                "terminal worker capacity exhausted".into(),
            )];
        }
        if let Err(error) = self.sessions.insert(spec.session.clone(), caller, mode) {
            return vec![Action::CreateFailed(reply, error)];
        }
        self.pending.insert(spec.session.clone(), reply);
        vec![Action::Create(spec)]
    }

    fn drive(
        &mut self,
        session: String,
        caller: Caller,
        kind: Write,
        command: wire::Command,
        reply: Reply,
    ) -> Vec<Action> {
        if let Err(error) = self.sessions.write(&session, &caller, kind) {
            return vec![Action::Reply(reply, Err(error))];
        }
        match kind {
            Write::Input | Write::Resize => vec![Action::Drive { command, reply }],
            Write::Close => {
                self.sessions.closing(&session);
                vec![Action::Close {
                    session,
                    reply: Some(reply),
                }]
            }
        }
    }

    fn replay(
        &self,
        session: String,
        caller: Caller,
        after: u64,
        reply: oneshot::Sender<Result<Replay, String>>,
    ) -> Vec<Action> {
        vec![Action::Replay(
            reply,
            self.sessions.replay(&session, &caller, after),
        )]
    }

    fn stop() -> Vec<Action> {
        vec![Action::Stop]
    }

    fn effect(&mut self, effect: Effect) -> Vec<Action> {
        match effect {
            Effect::Created(session) => self.created(session),
            Effect::Refused { session, reason } => self.refused(session, reason),
            Effect::Changed(_) => Self::changed(),
            Effect::Close(session) => self.close(session),
            Effect::None => Self::unchanged(),
        }
    }

    fn created(&mut self, session: String) -> Vec<Action> {
        match self.pending.remove(&session) {
            Some(reply) => vec![Action::Created { session, reply }],
            None => self.close(session),
        }
    }

    fn refused(&mut self, session: String, reason: wire::Refusal) -> Vec<Action> {
        let mut actions = vec![Action::Changed];
        if let Some(reply) = self.pending.remove(&session) {
            actions.push(Action::CreateFailed(reply, reason.token().into()));
        }
        actions
    }

    fn changed() -> Vec<Action> {
        vec![Action::Changed]
    }
    fn unchanged() -> Vec<Action> {
        Vec::new()
    }

    fn close(&mut self, session: String) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Some(reply) = self.pending.remove(&session) {
            actions.push(Action::CreateFailed(
                reply,
                "session ended during create".into(),
            ));
        }
        self.sessions.closing(&session);
        actions.push(Action::Close {
            session,
            reply: None,
        });
        actions
    }
}

impl Runtime {
    /// The caller owns the returned service task. Dropping all handles or
    /// requesting stop closes PTYs before that task finishes.
    pub fn start(
        providers: provider_host::ProviderSet,
        identity: String,
        directory: PathBuf,
    ) -> (Self, JoinHandle<Result<(), String>>) {
        let (requests, receiver) = mpsc::channel(LANE);
        let (changes, notifications) = watch::channel(());
        let (events, event_rx) = mpsc::channel(LANE);
        let engine = agent_service::Sessions::new(providers, identity, directory, events);
        let task = tokio::spawn(run(engine, receiver, event_rx, changes));
        (
            Self {
                requests,
                changes: notifications,
            },
            task,
        )
    }

    /// Subscribe BEFORE reading replay so output arriving during the read
    /// always leaves a notification for the next read.
    pub fn changes(&self) -> watch::Receiver<()> {
        self.changes.clone()
    }

    /// Accepts an already-admitted executor specification, never raw client
    /// credentials. Request cancellation does not cancel the spawn job: its
    /// eventual completion is observed and compensated with a close.
    pub async fn create(
        &self,
        caller: Caller,
        mode: Mode,
        spec: wire::Create,
    ) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.send(Request::Create {
            caller,
            mode,
            spec,
            reply,
        })
        .await?;
        let accepted = result.await.map_err(|_| "terminal runtime stopped")??;
        accepted
            .send(())
            .map_err(|_| "terminal runtime stopped".into())
    }

    pub async fn input(
        &self,
        session: String,
        caller: Caller,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        if bytes.len() > MAX_INPUT_BYTES {
            return Err("terminal input too large".into());
        }
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let command = wire::Command::TermInput {
            session: session.clone(),
            data_b64,
        };
        self.drive(session, caller, Write::Input, command).await
    }

    pub async fn resize(
        &self,
        session: String,
        caller: Caller,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        let invalid_size = cols == 0 || rows == 0;
        if invalid_size {
            return Err("invalid terminal size".into());
        }
        let command = wire::Command::TermResize {
            session: session.clone(),
            cols,
            rows,
        };
        self.drive(session, caller, Write::Resize, command).await
    }

    pub async fn close(&self, session: String, caller: Caller) -> Result<(), String> {
        let command = wire::Command::TermClose {
            session: session.clone(),
        };
        self.drive(session, caller, Write::Close, command).await
    }

    async fn drive(
        &self,
        session: String,
        caller: Caller,
        kind: Write,
        command: wire::Command,
    ) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.send(Request::Drive {
            session,
            caller,
            kind,
            command,
            reply,
        })
        .await?;
        result.await.map_err(|_| "terminal runtime stopped")?
    }

    pub async fn replay(
        &self,
        session: String,
        caller: Caller,
        after: u64,
    ) -> Result<Replay, String> {
        let (reply, result) = oneshot::channel();
        self.send(Request::Replay {
            session,
            caller,
            after,
            reply,
        })
        .await?;
        result.await.map_err(|_| "terminal runtime stopped")?
    }

    pub async fn stop(&self) -> Result<(), String> {
        self.send(Request::Stop).await
    }

    async fn send(&self, request: Request) -> Result<(), String> {
        self.requests
            .send(request)
            .await
            .map_err(|_| "terminal runtime stopped".into())
    }
}

fn dispatch(
    engine: &agent_service::Sessions,
    jobs: &mut JoinSet<Option<String>>,
    command: wire::Command,
    reply: Option<Reply>,
) {
    let engine = engine.clone();
    jobs.spawn(async move {
        let result = match engine.dispatch(command).await {
            Some(refusal) => Err(format!("executor refused: {refusal:?}")),
            None => Ok(()),
        };
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
        None
    });
}

async fn execute(
    actions: Vec<Action>,
    engine: &agent_service::Sessions,
    creates: &mut JoinSet<Option<String>>,
    closes: &mut JoinSet<Option<String>>,
    changes: &watch::Sender<()>,
) -> bool {
    for action in actions {
        match action {
            Action::Create(spec) => {
                dispatch(engine, creates, wire::Command::TermCreate(spec), None)
            }
            Action::Close { session, reply } => {
                dispatch(engine, closes, wire::Command::TermClose { session }, reply)
            }
            Action::Drive { command, reply } => {
                // Input and resize only enqueue into the engine's bounded
                // per-session lane. Keep their request order on this task.
                let result = match engine.dispatch(command).await {
                    Some(refusal) => Err(format!("executor refused: {refusal:?}")),
                    None => Ok(()),
                };
                let _ = reply.send(result);
            }
            Action::Reply(reply, result) => {
                let _ = reply.send(result);
            }
            Action::Created { session, reply } => {
                let (accepted, acknowledgment) = oneshot::channel();
                let _ = reply.send(Ok(accepted));
                creates.spawn(async move { acknowledgment.await.err().map(|_| session) });
            }
            Action::CreateFailed(reply, error) => {
                let _ = reply.send(Err(error));
            }
            Action::Replay(reply, result) => {
                let _ = reply.send(result);
            }
            Action::Changed => {
                changes.send_replace(());
            }
            Action::Stop => return false,
        }
    }
    true
}

async fn run(
    engine: agent_service::Sessions,
    mut requests: mpsc::Receiver<Request>,
    mut events: mpsc::Receiver<wire::Event>,
    changes: watch::Sender<()>,
) -> Result<(), String> {
    let mut machine = Machine::default();
    let mut creates = JoinSet::new();
    let mut closes = JoinSet::new();
    let result = loop {
        let workers = creates.len() + closes.len();
        let creating = !creates.is_empty();
        let closing = !closes.is_empty();
        let actions = tokio::select! {
            request = requests.recv() => match request {
                Some(request) => machine.request(request, workers),
                None => break Ok(()),
            },
            event = events.recv() => match event {
                Some(event) => match machine.sessions.on_engine(event) {
                    Ok(effect) => machine.effect(effect),
                    Err(error) => break Err(error),
                },
                None => break Err("terminal executor event stream ended".into()),
            },
            finished = creates.join_next(), if creating => {
                match finished {
                    Some(Ok(Some(session))) => machine.close(session),
                    Some(Ok(None)) | None => Vec::new(),
                    Some(Err(error)) => break Err(format!("terminal executor task failed: {error}")),
                }
            },
            finished = closes.join_next(), if closing => {
                if let Some(Err(error)) = finished { break Err(format!("terminal close task failed: {error}")); }
                Vec::new()
            }
        };
        if !execute(actions, &engine, &mut creates, &mut closes, &changes).await {
            break Ok(());
        }
    };
    requests.close();
    // Cancel outstanding spawns before sweeping, so none can register after
    // close_all takes its snapshot. Keep draining executor output during the
    // sweep: close emits an event before waiting for the child to terminate.
    creates.abort_all();
    while creates.join_next().await.is_some() {}
    let close = async {
        engine.close_all().await;
        // A close may already have removed its record from the engine while
        // still waiting for the process. Never abort that teardown job.
        while let Some(finished) = closes.join_next().await {
            finished.map_err(|error| format!("terminal close task failed: {error}"))?;
        }
        Ok::<(), String>(())
    };
    tokio::pin!(close);
    loop {
        tokio::select! {
            closed = &mut close => break result.and(closed),
            _ = events.recv() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn successful_reply_send_keeps_ownership_until_the_caller_acknowledges() {
        let directory = tempfile::tempdir().unwrap();
        let (events, _rx) = mpsc::channel(1);
        let engine = agent_service::Sessions::new(
            provider_host::ProviderSet::empty(),
            "test".into(),
            directory.path().into(),
            events,
        );
        let (reply, receiver) = oneshot::channel();
        let (changes, _) = watch::channel(());
        let mut creates = JoinSet::new();
        let mut closes = JoinSet::new();
        execute(
            vec![Action::Created {
                session: "0000000000000001".into(),
                reply,
            }],
            &engine,
            &mut creates,
            &mut closes,
            &changes,
        )
        .await;
        assert!(
            !creates.is_empty(),
            "a sent reply is not an accepted session"
        );
        drop(receiver);
        assert_eq!(
            creates.join_next().await.unwrap().unwrap(),
            Some("0000000000000001".into())
        );
    }
}
