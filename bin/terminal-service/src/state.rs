use agent_service::wire;
use base64::Engine as _;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    Account { account: u64, node: [u8; 32] },
    Operator { node: [u8; 32] },
}

#[derive(Clone, Copy)]
pub enum Write {
    Input,
    Resize,
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Cancelling,
    Running,
    Ended,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Created {
    Ready,
    Close,
}

/// Work the service performs after recording an executor event.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Created(String),
    Refused {
        session: String,
        reason: wire::Refusal,
    },
    Changed(String),
    Close(String),
    None,
}

#[derive(Clone)]
pub struct Chunk {
    pub seq: u64,
    pub bytes: Vec<u8>,
}

pub struct Replay {
    pub first: u64,
    pub head: u64,
    pub chunks: Vec<Chunk>,
    pub ended: bool,
}

struct Record {
    owner: Caller,
    phase: Phase,
    chunks: VecDeque<Chunk>,
    head: u64,
}

#[derive(Default)]
pub struct Sessions {
    records: BTreeMap<String, Record>,
}

impl Sessions {
    /// Executor facts update session state before the service answers a client
    /// or dispatches a compensating close. Messaging events belong to another
    /// service and cannot enter this terminal event stream.
    pub fn on_engine(&mut self, event: wire::Event) -> Result<Effect, String> {
        match event {
            wire::Event::TermCreated { session } => self.engine_created(session),
            // `detail` is this host's own diagnosis of the failed spawn, not the
            // client's answer: the session engine logs it at the point it
            // decides the refusal, and the stable token is what crosses back.
            wire::Event::TermRefused {
                session,
                reason,
                detail: _,
            } => self.engine_refused(session, reason),
            wire::Event::TermOutput { session, chunk_b64 } => {
                self.engine_output(session, chunk_b64)
            }
            wire::Event::TermEnded { session } => self.engine_ended(session),
            wire::Event::MsgBound { .. } => Self::unexpected_message(),
            wire::Event::MsgBindRefused { .. } => Self::unexpected_message(),
            wire::Event::MsgDelivery { .. } => Self::unexpected_message(),
        }
    }

    fn engine_created(&mut self, session: String) -> Result<Effect, String> {
        let effect = match self.created(&session) {
            Created::Ready => Effect::Created(session),
            Created::Close => Effect::Close(session),
        };
        Ok(effect)
    }

    fn engine_refused(&mut self, session: String, reason: wire::Refusal) -> Result<Effect, String> {
        let changed = self.end(&session);
        if !changed {
            return Ok(Effect::None);
        }
        Ok(Effect::Refused { session, reason })
    }

    fn engine_output(&mut self, session: String, chunk_b64: String) -> Result<Effect, String> {
        // The executor pump can have a read in flight when teardown emits
        // Ended. Its late bytes cannot reopen a record or stop other sessions;
        // an unknown session has the same terminal outcome.
        let accepts_output = self
            .records
            .get(&session)
            .is_some_and(|record| record.phase != Phase::Ended);
        if !accepts_output {
            return Ok(Effect::None);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(chunk_b64)
            .map_err(|_| "invalid executor output encoding")?;
        self.output(&session, bytes)?;
        Ok(Effect::Changed(session))
    }

    fn engine_ended(&mut self, session: String) -> Result<Effect, String> {
        let changed = self.end(&session);
        if !changed {
            return Ok(Effect::None);
        }
        Ok(Effect::Changed(session))
    }

    fn unexpected_message() -> Result<Effect, String> {
        Err("messaging event on terminal executor stream".into())
    }

    pub fn insert(&mut self, id: String, owner: Caller) -> Result<(), String> {
        let invalid = !agent_service::wire::valid_session(&id) || self.records.contains_key(&id);
        if invalid {
            return Err("invalid or occupied session".into());
        }
        self.records.insert(
            id,
            Record {
                owner,
                phase: Phase::Starting,
                chunks: VecDeque::new(),
                head: 0,
            },
        );
        Ok(())
    }

    pub fn created(&mut self, id: &str) -> Created {
        let Some(record) = self.records.get_mut(id) else {
            return Created::Close;
        };
        match record.phase {
            Phase::Starting => {
                record.phase = Phase::Running;
                Created::Ready
            }
            Phase::Running => Created::Ready,
            Phase::Cancelling | Phase::Ended => Created::Close,
        }
    }

    /// Refuse further writes as soon as an accepted close is scheduled.
    pub fn closing(&mut self, id: &str) {
        let Some(record) = self.records.get_mut(id) else {
            return;
        };
        if record.phase != Phase::Ended {
            record.phase = Phase::Cancelling;
        }
    }

    pub fn cancel_create(&mut self, id: &str) {
        let Some(record) = self.records.get_mut(id) else {
            return;
        };
        if record.phase == Phase::Starting {
            record.phase = Phase::Cancelling;
        }
    }

    /// A session answers exactly one caller: the operator that created it.
    pub fn read(&self, id: &str, caller: &Caller) -> Result<(), String> {
        let record = self.records.get(id).ok_or("unknown session")?;
        if record.owner != *caller {
            return Err("session is not readable by this caller".into());
        }
        Ok(())
    }

    pub fn write(&self, id: &str, caller: &Caller) -> Result<(), String> {
        let record = self.records.get(id).ok_or("unknown session")?;
        if record.owner != *caller {
            return Err("session belongs to another caller".into());
        }
        if record.phase != Phase::Running {
            return Err("session is not running".into());
        }
        Ok(())
    }

    pub fn output(&mut self, id: &str, bytes: Vec<u8>) -> Result<(), String> {
        let record = self.records.get_mut(id).ok_or("unknown session")?;
        let invalid_size = bytes.is_empty();
        let rejected = record.phase == Phase::Ended || invalid_size;
        if rejected {
            return Err("closed session or invalid output size".into());
        }
        record.head += 1;
        record.chunks.push_back(Chunk {
            seq: record.head,
            bytes,
        });
        Ok(())
    }

    pub fn replay(&self, id: &str, caller: &Caller, after: u64) -> Result<Replay, String> {
        self.read(id, caller)?;
        let record = &self.records[id];
        if after > record.head {
            return Err("resume cursor is ahead of this session".into());
        }
        // The queue is append-only with strictly increasing sequence numbers,
        // so a resume point is a binary search. Scanning for it instead costs
        // the whole retained stream on every reader wake-up, which is quadratic
        // over the life of a streaming session.
        let resume_chunk = record.chunks.partition_point(|chunk| chunk.seq <= after);
        Ok(Replay {
            first: record
                .chunks
                .front()
                .map_or(record.head + 1, |chunk| chunk.seq),
            head: record.head,
            chunks: record.chunks.range(resume_chunk..).cloned().collect(),
            ended: record.phase == Phase::Ended,
        })
    }

    pub fn end(&mut self, id: &str) -> bool {
        let Some(record) = self.records.get_mut(id) else {
            return false;
        };
        if record.phase == Phase::Ended {
            return false;
        }
        record.phase = Phase::Ended;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(account: u64, node: u8) -> Caller {
        Caller::Account {
            account,
            node: [node; 32],
        }
    }

    #[test]
    fn operator_sessions_require_the_same_node_operator_not_an_account() {
        let mut sessions = Sessions::default();
        let owner = Caller::Operator { node: [1; 32] };
        let id = "0000000000000001";
        sessions.insert(id.into(), owner.clone()).unwrap();
        sessions.created(id);
        assert!(sessions.write(id, &owner).is_ok());
        for stranger in [Caller::Operator { node: [2; 32] }, caller(7, 1)] {
            assert!(sessions.write(id, &stranger).is_err());
            assert!(sessions.replay(id, &stranger, 0).is_err());
        }
    }

    /// A session is one operator's: the exact account AND the exact node that
    /// created it drive and read it, and nobody else does either.
    #[test]
    fn a_session_answers_only_its_exact_creator() {
        let owner = caller(7, 1);
        let mut sessions = Sessions::default();
        let id = "0000000000000001";
        sessions.insert(id.into(), owner.clone()).unwrap();
        sessions.created(id);
        assert!(sessions.write(id, &owner).is_ok());
        assert!(sessions.read(id, &owner).is_ok());
        for stranger in [caller(7, 2), caller(8, 1), caller(8, 2)] {
            assert!(sessions.write(id, &stranger).is_err());
            assert!(sessions.read(id, &stranger).is_err());
        }
    }

    #[test]
    fn replay_retains_output_and_marks_end_once() {
        let owner = caller(7, 1);
        let mut sessions = Sessions::default();
        let id = "0000000000000001";
        sessions.insert(id.into(), owner.clone()).unwrap();
        for _ in 0..5 {
            sessions.output(id, vec![b'x'; 128 * 1024]).unwrap();
        }
        let replay = sessions.replay(id, &owner, 0).unwrap();
        assert_eq!(replay.first, 1);
        assert_eq!(replay.chunks.len(), 5);
        assert_eq!(replay.head, 5);
        assert!(!replay.ended);
        assert!(sessions.end(id));
        assert!(!sessions.end(id));
        assert!(sessions.replay(id, &owner, 5).unwrap().ended);
        assert!(sessions.output(id, vec![0]).is_err());
    }

    #[test]
    fn empty_output_does_not_advance_replay() {
        let owner = caller(7, 1);
        let mut sessions = Sessions::default();
        let id = "0000000000000001";
        sessions.insert(id.into(), owner.clone()).unwrap();
        assert!(sessions.output(id, Vec::new()).is_err());
        let replay = sessions.replay(id, &owner, 0).unwrap();
        assert_eq!(replay.head, 0);
        assert!(replay.chunks.is_empty());
    }

    #[test]
    fn new_sessions_preserve_live_and_ended_records() {
        let owner = caller(7, 1);
        let mut sessions = Sessions::default();
        for number in 0..128 {
            sessions
                .insert(format!("{number:016x}"), owner.clone())
                .unwrap();
        }
        let ended = "0000000000000001";
        sessions.created(ended);
        assert!(sessions.end(ended));
        sessions
            .insert("0000000000000080".into(), owner.clone())
            .unwrap();
        assert!(sessions.replay(ended, &owner, 0).unwrap().ended);
        assert!(sessions.replay("0000000000000000", &owner, 0).is_ok());
        assert!(sessions.replay("0000000000000080", &owner, 1).is_err());
    }

    struct PtyProvider;

    #[async_trait::async_trait]
    impl provider_host::Provider for PtyProvider {
        fn capability(&self) -> &str {
            "stub"
        }

        async fn run(&self, _: &str, _: &provider_host::RunContext) -> Result<String, String> {
            Err("interactive-only test provider".into())
        }

        async fn spawn_interactive(
            &self,
            _: &provider_host::RunContext,
            _: bool,
        ) -> Result<provider_host::InteractiveSession, String> {
            provider_host::InteractiveSession::spawn_local(tokio::process::Command::new("cat"))
        }
    }

    #[tokio::test]
    async fn cancelled_create_closes_the_real_executor_pty_and_workdir() {
        let directory = tempfile::tempdir().unwrap();
        let spec = provider_host::CapabilitySpec::parse(
            r#"
            spec = 1
            [capability]
            tag = "stub"
            description = "lifecycle test"
            [detect]
            bin = "cat"
            [invoke]
            args = []
            prompt = "stdin"
            [output]
            format = "text"
        "#,
            "test",
        )
        .unwrap();
        let providers = provider_host::ProviderSet::assemble(
            provider_host::SpecSet::from_specs(vec![spec]),
            vec![Box::new(PtyProvider)],
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let engine = agent_service::Sessions::new(
            providers,
            "test-service".into(),
            directory.path().into(),
            tx,
        );
        let mut sessions = Sessions::default();
        let owner = caller(7, 1);
        let session = "0000000000000001".to_string();
        sessions.insert(session.clone(), owner.clone()).unwrap();
        sessions.cancel_create(&session);
        engine
            .dispatch(wire::Command::TermCreate(wire::Create {
                session: session.clone(),
                provider: "stub".into(),
                restricted: false,
                limits: BTreeMap::new(),
                credential: None,
            }))
            .await;
        assert_eq!(engine.live(), 1);
        assert!(directory.path().join(&session).is_dir());
        let effect = sessions.on_engine(rx.recv().await.unwrap()).unwrap();
        assert_eq!(effect, Effect::Close(session.clone()));
        let Effect::Close(id) = effect else {
            panic!("compensating close");
        };
        engine
            .dispatch(wire::Command::TermClose { session: id })
            .await;
        assert_eq!(
            sessions.on_engine(rx.recv().await.unwrap()).unwrap(),
            Effect::Changed(session.clone())
        );
        assert_eq!(engine.live(), 0);
        assert!(sessions.replay(&session, &owner, 0).unwrap().ended);
        assert!(!directory.path().join(session).exists());
    }

    #[tokio::test]
    async fn executor_refusal_finishes_each_record() {
        let directory = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let engine = agent_service::Sessions::new(
            provider_host::ProviderSet::empty(),
            "test-service".into(),
            directory.path().into(),
            tx,
        );
        let owner = caller(7, 1);
        let mut sessions = Sessions::default();
        for number in 0..32 {
            let session = format!("{number:016x}");
            sessions.insert(session.clone(), owner.clone()).unwrap();
            engine
                .dispatch(wire::Command::TermCreate(wire::Create {
                    session: session.clone(),
                    provider: "absent".into(),
                    restricted: false,
                    limits: BTreeMap::new(),
                    credential: None,
                }))
                .await;
            let effect = sessions.on_engine(rx.recv().await.unwrap()).unwrap();
            assert_eq!(
                effect,
                Effect::Refused {
                    session: session.clone(),
                    reason: wire::Refusal::UnknownProvider
                }
            );
            assert!(sessions.replay(&session, &owner, 0).unwrap().ended);
        }
        assert_eq!(engine.live(), 0);
    }

    #[test]
    fn late_output_from_an_ended_session_does_not_end_its_sibling() {
        let mut sessions = Sessions::default();
        let owner = caller(7, 1);
        let ended = "0000000000000001";
        let active = "0000000000000002";
        for session in [ended, active] {
            sessions.insert(session.into(), owner.clone()).unwrap();
            sessions.created(session);
        }
        sessions.end(ended);
        assert_eq!(
            sessions
                .on_engine(wire::Event::TermOutput {
                    session: ended.into(),
                    chunk_b64: "bGF0ZQ==".into()
                })
                .unwrap(),
            Effect::None
        );
        assert!(sessions.write(active, &owner).is_ok());
        assert!(sessions.replay(ended, &owner, 0).unwrap().chunks.is_empty());
    }

    #[test]
    fn cancelled_create_keeps_output_order_but_requests_executor_teardown() {
        let mut sessions = Sessions::default();
        let owner = caller(7, 1);
        let session = "0000000000000001".to_string();
        sessions.insert(session.clone(), owner.clone()).unwrap();
        sessions.cancel_create(&session);
        let output = wire::Event::TermOutput {
            session: session.clone(),
            chunk_b64: "aGk=".into(),
        };
        assert_eq!(
            sessions.on_engine(output).unwrap(),
            Effect::Changed(session.clone())
        );
        assert_eq!(
            sessions
                .on_engine(wire::Event::TermCreated {
                    session: session.clone()
                })
                .unwrap(),
            Effect::Close(session.clone())
        );
        assert!(sessions.write(&session, &owner).is_err());
        assert_eq!(
            sessions
                .on_engine(wire::Event::TermEnded {
                    session: session.clone()
                })
                .unwrap(),
            Effect::Changed(session.clone())
        );
        assert_eq!(
            sessions
                .on_engine(wire::Event::TermEnded {
                    session: session.clone()
                })
                .unwrap(),
            Effect::None
        );
        let replay = sessions.replay(&session, &owner, 0).unwrap();
        assert!(replay.ended);
        assert_eq!(replay.chunks[0].bytes, b"hi");
    }

    /// The resume point is found by binary search, which is only correct while
    /// the queue stays sorted by a strictly increasing sequence. Resuming from
    /// every cursor a reader can hold must return exactly the tail after it.
    #[test]
    fn every_resume_cursor_returns_exactly_the_tail_after_it() {
        let owner = caller(7, 1);
        let mut sessions = Sessions::default();
        let id = "0000000000000001";
        sessions.insert(id.into(), owner.clone()).unwrap();
        sessions.created(id);
        for _ in 0..512 {
            sessions.output(id, vec![7]).unwrap();
        }
        let full = sessions.replay(id, &owner, 0).unwrap();
        assert_eq!(full.chunks.len(), 512);
        assert_eq!(full.head, 512);
        for after in 0..=512u64 {
            let page = sessions.replay(id, &owner, after).unwrap();
            let expected: Vec<u64> = (after + 1..=512).collect();
            let seqs: Vec<u64> = page.chunks.iter().map(|chunk| chunk.seq).collect();
            assert_eq!(seqs, expected, "output resume from {after}");
        }
        assert!(sessions.replay(id, &owner, 513).is_err());
    }

    #[test]
    fn tiny_output_chunks_remain_available_for_replay() {
        let owner = caller(7, 1);
        let mut sessions = Sessions::default();
        let id = "0000000000000001";
        sessions.insert(id.into(), owner.clone()).unwrap();
        sessions.created(id);
        for _ in 0..2048 {
            sessions.output(id, vec![1]).unwrap();
        }
        let replay = sessions.replay(id, &owner, 0).unwrap();
        assert_eq!(replay.chunks.len(), 2048);
        assert_eq!(replay.first, 1);
        assert_eq!(replay.head, 2048);
    }

    #[test]
    fn cancelled_pending_create_is_closed_when_the_engine_finishes() {
        let owner = caller(7, 1);
        let mut sessions = Sessions::default();
        let id = "0000000000000001";
        sessions.insert(id.into(), owner).unwrap();
        sessions.cancel_create(id);
        assert_eq!(sessions.created(id), Created::Close);
        assert_eq!(sessions.created("0000000000000002"), Created::Close);
    }
}
