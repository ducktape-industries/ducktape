use std::collections::BTreeMap;

use abi::{
    BlobId, Cause, Env, GuestCall, HostOp, HostReply, ItemRef, Message, Origin, Outcome, ProgramId,
    Refusal, reason,
};
use blobs::{Blobs, Layered, Stage};
use commonware_runtime::Spawner;
use commonware_storage::Context;
use runtime::{Code, Fault, Limits, Runtime};
use state::{Overlay, Storage, Store, View};

use crate::{Error, Receipt, Result, crypto, namespace, queue};

pub struct Loaded {
    runtime: Runtime,
    programs: BTreeMap<ProgramId, Slot>,
}

enum Slot {
    Running { code: BlobId, module: Code },
    Awaiting { code: BlobId },
}

impl Loaded {
    pub fn new(limits: Limits) -> Loaded {
        Loaded {
            runtime: Runtime::new(limits),
            programs: BTreeMap::new(),
        }
    }

    pub fn limits(&self) -> Limits {
        self.runtime.limits()
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn module(&self, program: &str) -> Option<&Code> {
        match self.programs.get(program)? {
            Slot::Running { module, .. } => Some(module),
            Slot::Awaiting { .. } => None,
        }
    }

    fn running(&self, program: &str) -> Option<&BlobId> {
        match self.programs.get(program)? {
            Slot::Running { code, .. } => Some(code),
            Slot::Awaiting { .. } => None,
        }
    }

    pub fn awaiting(&self, program: &str) -> Option<BlobId> {
        match self.programs.get(program)? {
            Slot::Running { .. } => None,
            Slot::Awaiting { code } => Some(*code),
        }
    }

    pub fn missing(&self) -> Vec<(ProgramId, BlobId)> {
        self.programs
            .iter()
            .filter_map(|(program, slot)| match slot {
                Slot::Running { .. } => None,
                Slot::Awaiting { code } => Some((program.clone(), *code)),
            })
            .collect()
    }

    pub fn load(
        &self,
        blobs: &Layered<'_>,
        id: &BlobId,
    ) -> Result<Option<std::result::Result<Code, Fault>>> {
        let Some(framed) = blobs.framed(id)? else {
            return Ok(None);
        };
        let (_, body) = blobs::parse(&framed).ok_or_else(|| {
            Error::Blobs(blobs::Error::Corrupt(
                *id,
                "the frame does not parse".into(),
            ))
        })?;
        Ok(Some(self.runtime.load(body)))
    }

    pub fn admit(&mut self, program: ProgramId, code: BlobId, module: Code) {
        self.programs
            .insert(program, Slot::Running { code, module });
    }

    pub fn unload(&mut self, program: &str) {
        self.programs.remove(program);
    }

    pub fn sync(&mut self, wanted: &BTreeMap<ProgramId, BlobId>, blobs: &Blobs) -> Result<()> {
        self.programs
            .retain(|program, _| wanted.contains_key(program));
        let stage = Stage::default();
        let layered = Layered {
            stage: &stage,
            blobs,
        };
        for (program, code) in wanted {
            let running_this_code = self.running(program) == Some(code);
            if running_this_code {
                continue;
            }
            let slot = match self.load(&layered, code)? {
                None => Slot::Awaiting { code: *code },
                Some(Err(fault)) => {
                    return Err(Error::Load {
                        program: program.clone(),
                        code: *code,
                        fault,
                    });
                }
                Some(Ok(module)) => Slot::Running {
                    code: *code,
                    module,
                },
            };
            self.programs.insert(program.clone(), slot);
        }
        Ok(())
    }
}

pub struct World<'a, E>
where
    E: Context + Spawner,
{
    pub store: &'a Store<E>,
    pub blobs: &'a Blobs,
    pub loaded: &'a Loaded,
    pub height: u64,
    pub time: u64,
}

impl<E> Clone for World<'_, E>
where
    E: Context + Spawner,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for World<'_, E> where E: Context + Spawner {}

pub fn refusal_of(fault: Fault) -> Refusal {
    match fault {
        Fault::Load(sentence) => Refusal::new(reason::UNSUPPORTED, sentence),
        Fault::Trap(sentence) => Refusal::new(reason::TRAP, sentence),
        Fault::Protocol(sentence) => Refusal::new(reason::PROTOCOL, sentence),
    }
}

fn unknown(program: &str) -> Refusal {
    Refusal::new(reason::UNKNOWN_PROGRAM, program)
}

fn host_fault() -> HostReply {
    HostReply::Refused(Refusal::new(
        reason::UNSUPPORTED,
        "this node cannot serve the request",
    ))
}

struct Reader<'r, E>
where
    E: Context + Spawner,
{
    world: World<'r, E>,
    layers: Vec<&'r Overlay>,
    stage: &'r Stage,
    env: &'r Env,
    stack: &'r [ProgramId],
}

impl<'r, E> Reader<'r, E>
where
    E: Context + Spawner,
{
    fn view(&self) -> View<'r> {
        self.world.store.view(self.layers.clone())
    }

    fn confirmed(&self) -> View<'r> {
        self.world.store.view(Vec::new())
    }

    fn blob<T>(
        &self,
        id: &BlobId,
        read: impl FnOnce(&Layered<'r>) -> blobs::Result<Option<T>>,
    ) -> Result<Option<T>> {
        let rostered = self
            .view()
            .get(namespace::BLOBS, &namespace::blob(id))?
            .is_some();
        if !rostered {
            return Ok(None);
        }
        let layered = Layered {
            stage: self.stage,
            blobs: self.world.blobs,
        };
        read(&layered)?.map(Some).ok_or(Error::BlobUnavailable(*id))
    }

    async fn serve(&self, op: HostOp) -> Result<HostReply> {
        let me = self.env.me.as_str();
        let reply = match op {
            HostOp::Env => HostReply::Env(self.env.clone()),
            HostOp::Get(key) => HostReply::Value(self.view().get(me, &key)?),
            HostOp::Scan(scan) => HostReply::Entries(self.view().scan(me, &scan)?),
            HostOp::CommittedGet(key) => HostReply::Value(self.confirmed().get(me, &key)?),
            HostOp::CommittedScan(scan) => HostReply::Entries(self.confirmed().scan(me, &scan)?),
            HostOp::BlobGet(id) => HostReply::Blob(self.blob(&id, |layered| layered.get(&id))?),
            HostOp::BlobStat(id) => {
                HostReply::BlobHeader(self.blob(&id, |layered| layered.stat(&id))?)
            }
            HostOp::BlobRead { id, offset, len } => {
                HostReply::Value(self.blob(&id, |layered| layered.read(&id, offset, len))?)
            }
            HostOp::Root(program) => HostReply::Root(self.world.store.root(&program)?),
            HostOp::Query { program, request } => HostReply::Query(
                query(
                    self.world,
                    self.layers.clone(),
                    self.stage,
                    self.stack,
                    Origin::Program(self.env.me.clone()),
                    program,
                    request,
                )
                .await?,
            ),
            HostOp::Crypto(op) => crypto::serve(op),
            HostOp::Set { .. }
            | HostOp::Delete(_)
            | HostOp::BlobPut { .. }
            | HostOp::Emit(_)
            | HostOp::Event(_)
            | HostOp::Output(_) => {
                HostReply::Refused(Refusal::new(reason::UNSUPPORTED, "a query does not write"))
            }
        };
        Ok(reply)
    }
}

struct Query<'a, E>
where
    E: Context + Spawner,
{
    world: World<'a, E>,
    env: Env,
    layers: Vec<&'a Overlay>,
    stage: &'a Stage,
    stack: Vec<ProgramId>,
    fault: Option<Error>,
}

#[async_trait::async_trait(?Send)]
impl<E> runtime::Host for Query<'_, E>
where
    E: Context + Spawner,
{
    async fn call(&mut self, op: HostOp) -> HostReply {
        let result = {
            let reader = Reader {
                world: self.world,
                layers: self.layers.clone(),
                stage: self.stage,
                env: &self.env,
                stack: &self.stack,
            };
            reader.serve(op).await
        };
        match result {
            Ok(reply) => reply,
            Err(error) => {
                self.fault = Some(error);
                host_fault()
            }
        }
    }
}

pub async fn query<'a, E>(
    world: World<'a, E>,
    layers: Vec<&'a Overlay>,
    stage: &'a Stage,
    stack: &[ProgramId],
    origin: Origin,
    program: ProgramId,
    request: Vec<u8>,
) -> Result<std::result::Result<Vec<u8>, Refusal>>
where
    E: Context + Spawner,
{
    let Some(module) = world.loaded.module(&program) else {
        return Ok(Err(unknown(&program)));
    };
    let cycles = stack.contains(&program);
    if cycles {
        return Ok(Err(Refusal::new(
            reason::PROTOCOL,
            format!("{program} is already answering a query on this stack"),
        )));
    }
    let env = Env {
        height: world.height,
        time: world.time,
        me: program.clone(),
        origin,
        cause: Cause::Direct,
    };
    let mut stack = stack.to_vec();
    stack.push(program);
    let mut unit = Query {
        world,
        env,
        layers,
        stage,
        stack,
        fault: None,
    };
    let verdict = world
        .loaded
        .runtime()
        .run(module, GuestCall::Query(request), &mut unit)
        .await;
    if let Some(fault) = unit.fault {
        return Err(fault);
    }
    Ok(match verdict {
        Ok(reply) => reply,
        Err(fault) => Err(refusal_of(fault)),
    })
}

struct Execute<'a, E>
where
    E: Context + Spawner,
{
    world: World<'a, E>,
    env: Env,
    overlay: &'a mut Overlay,
    stage: &'a mut Stage,
    events: Vec<Vec<u8>>,
    output: Vec<u8>,
    fault: Option<Error>,
}

impl<E> Execute<'_, E>
where
    E: Context + Spawner,
{
    fn put(&mut self, hash: abi::HashKind, kind: &str, body: &[u8]) -> HostReply {
        match self.stage.put(hash, kind, body) {
            Ok(id) => {
                self.overlay
                    .set(namespace::BLOBS, namespace::blob(&id), Vec::new());
                HostReply::BlobId(id)
            }
            Err(refusal) => HostReply::Refused(refusal),
        }
    }

    fn emit(&mut self, message: Message) -> Result<HostReply> {
        let source = self.env.me.clone();
        let item = queue::Item::Message {
            source: source.clone(),
            message,
        };
        let seq = queue::push(self.world.store.storage(), self.overlay, item)?;
        Ok(HostReply::Item(ItemRef { source, item: seq }))
    }
}

#[async_trait::async_trait(?Send)]
impl<E> runtime::Host for Execute<'_, E>
where
    E: Context + Spawner,
{
    async fn call(&mut self, op: HostOp) -> HostReply {
        let me = self.env.me.clone();
        let result = match op {
            HostOp::Set { key, value } => {
                self.overlay.set(&me, key, value);
                Ok(HostReply::Done)
            }
            HostOp::Delete(key) => {
                self.overlay.delete(&me, key);
                Ok(HostReply::Done)
            }
            HostOp::BlobPut { hash, kind, body } => Ok(self.put(hash, &kind, &body)),
            HostOp::Emit(message) => self.emit(message),
            HostOp::Event(bytes) => {
                self.events.push(bytes);
                Ok(HostReply::Done)
            }
            HostOp::Output(bytes) => {
                self.output = bytes;
                Ok(HostReply::Done)
            }
            other => {
                let reader = Reader {
                    world: self.world,
                    layers: vec![&*self.overlay],
                    stage: &*self.stage,
                    env: &self.env,
                    stack: &[],
                };
                reader.serve(other).await
            }
        };
        match result {
            Ok(reply) => reply,
            Err(error) => {
                self.fault = Some(error);
                host_fault()
            }
        }
    }
}

pub async fn execute<E>(
    world: World<'_, E>,
    overlay: &mut Overlay,
    stage: &mut Stage,
    program: &str,
    call: GuestCall,
    env: Env,
) -> Result<Receipt>
where
    E: Context + Spawner,
{
    let Some(module) = world.loaded.module(program) else {
        return Ok(Receipt {
            program: program.to_owned(),
            outcome: Outcome::Rejected(unknown(program)),
            events: Vec::new(),
        });
    };
    let checkpoint = overlay.checkpoint();
    let (verdict, events, output, fault) = {
        let mut unit = Execute {
            world,
            env,
            overlay: &mut *overlay,
            stage: &mut *stage,
            events: Vec::new(),
            output: Vec::new(),
            fault: None,
        };
        let verdict = world.loaded.runtime().run(module, call, &mut unit).await;
        (verdict, unit.events, unit.output, unit.fault)
    };
    if let Some(fault) = fault {
        return Err(fault);
    }
    let outcome = match verdict {
        Ok(Ok(_)) => Outcome::Applied { output },
        Ok(Err(refusal)) => Outcome::Rejected(refusal),
        Err(fault) => Outcome::Rejected(refusal_of(fault)),
    };
    if let Outcome::Rejected(_) = &outcome {
        overlay.restore(checkpoint);
        discard_unrostered(world.store.storage(), overlay, stage)?;
    }
    Ok(Receipt {
        program: program.to_owned(),
        outcome,
        events,
    })
}

fn discard_unrostered(storage: &Storage, overlay: &Overlay, stage: &mut Stage) -> Result<()> {
    let view = View::new(storage, vec![overlay]);
    let mut kept = Vec::new();
    for id in stage.ids() {
        let rostered = view.get(namespace::BLOBS, &namespace::blob(id))?.is_some();
        if rostered {
            kept.push(*id);
        }
    }
    stage.retain(|id| kept.contains(id));
    Ok(())
}
