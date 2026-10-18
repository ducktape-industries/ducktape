use std::collections::BTreeMap;

use abi::role::awaits::{Answer, FrameId, Ticket};
use abi::role::identity::AccountNumber;
use abi::role::ownership::{self, Table, Write};
use abi::{
    BlobId, Cause, Env, HostOp, HostReply, Invocation, ItemRef, Message, Origin, Outcome,
    ProgramCall, ProgramId, Receipt, Refusal, Roles, reason, role,
};
use blob::{Blobs, Layered, Stage};
use commonware_runtime::Spawner;
use commonware_storage::Context;
use runtime::{Code, Fault, Limits, Ran, Runtime, Suspended};
use state::{Overlay, Storage, Store, View};

use crate::{Error, Result, crypto, namespace, parked};

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

    /// The tables `program`'s running code declares; none for a program
    /// that does not run here.
    pub fn tables(&self, program: &str) -> &[Table] {
        self.module(program).map_or(&[], Code::tables)
    }

    pub fn code(&self, program: &str) -> Option<BlobId> {
        self.running(program).copied()
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
        let (_, body) = blob::parse(&framed).ok_or_else(|| {
            Error::Blobs(blob::Error::Corrupt(*id, "the frame does not parse".into()))
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
    pub network: &'a [u8],
    pub roles: &'a Roles,
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

pub(crate) fn unknown(program: &str) -> Refusal {
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

    async fn blob<T>(
        &self,
        id: &BlobId,
        fuel: &mut Option<u64>,
        read: impl FnOnce(&Layered<'r>) -> blob::Result<Option<T>>,
    ) -> Result<Option<T>> {
        let rostered = self
            .view()
            .get(namespace::BLOBS, &namespace::blob(id))?
            .is_some();
        let readable = rostered
            || held(
                self.world,
                self.layers.clone(),
                self.stage,
                self.stack,
                id,
                fuel,
            )
            .await?;
        if !readable {
            return Ok(None);
        }
        let layered = Layered {
            stage: self.stage,
            blobs: self.world.blobs,
        };
        read(&layered)?.map(Some).ok_or(Error::BlobUnavailable(*id))
    }

    /// Serves a read. A query it makes runs on `fuel`, the caller's.
    async fn serve(&self, op: HostOp, fuel: &mut Option<u64>) -> Result<HostReply> {
        let me = self.env.me.as_str();
        let reply = match op {
            HostOp::Get(key) => HostReply::Value(self.view().get(me, &key)?),
            HostOp::Scan(scan) => HostReply::Entries(self.view().scan(me, &scan)?),
            HostOp::CommittedGet(key) => HostReply::Value(self.confirmed().get(me, &key)?),
            HostOp::CommittedScan(scan) => HostReply::Entries(self.confirmed().scan(me, &scan)?),
            HostOp::BlobGet(id) => {
                HostReply::Blob(self.blob(&id, fuel, |layered| layered.get(&id)).await?)
            }
            HostOp::BlobStat(id) => {
                HostReply::BlobHeader(self.blob(&id, fuel, |layered| layered.stat(&id)).await?)
            }
            HostOp::BlobRead { id, offset, len } => HostReply::Value(
                self.blob(&id, fuel, |layered| layered.read(&id, offset, len))
                    .await?,
            ),
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
                    fuel,
                )
                .await?,
            ),
            HostOp::Resource { program, key } => HostReply::Query(
                resource(
                    self.world,
                    self.layers.clone(),
                    self.stage,
                    self.stack,
                    Origin::Program(self.env.me.clone()),
                    program,
                    key,
                    fuel,
                )
                .await?,
            ),
            HostOp::Crypto(op) => crypto::serve(op),
            HostOp::Await { .. } | HostOp::Defer | HostOp::Resume { .. } => {
                HostReply::Refused(Refusal::new(reason::UNSUPPORTED, "a query does not await"))
            }
            HostOp::Set { .. }
            | HostOp::Delete(_)
            | HostOp::BlobPut { .. }
            | HostOp::Emit(_)
            | HostOp::Event(_)
            | HostOp::Output(_)
            | HostOp::Respond(_) => {
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
    response: Vec<u8>,
    fault: Option<Error>,
}

#[async_trait::async_trait]
impl<E> runtime::Host for Query<'_, E>
where
    E: Context + Spawner,
{
    async fn call(&mut self, op: HostOp, fuel: &mut Option<u64>) -> HostReply {
        let result = match op {
            HostOp::Respond(bytes) => {
                self.response.extend(bytes);
                Ok(HostReply::Done)
            }
            other => {
                let reader = Reader {
                    world: self.world,
                    layers: self.layers.clone(),
                    stage: self.stage,
                    env: &self.env,
                    stack: &self.stack,
                };
                reader.serve(other, fuel).await
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

/// Runs `program`'s query on `fuel`, leaving what it did not burn: a query
/// made inside a frame runs on the frame's budget, one made from outside on
/// a budget of its own.
#[allow(clippy::too_many_arguments)]
pub async fn query<'a, E>(
    world: World<'a, E>,
    layers: Vec<&'a Overlay>,
    stage: &'a Stage,
    stack: &[ProgramId],
    origin: Origin,
    program: ProgramId,
    request: Vec<u8>,
    fuel: &mut Option<u64>,
) -> Result<std::result::Result<Vec<u8>, Refusal>>
where
    E: Context + Spawner,
{
    let call = ProgramCall::Query(request);
    ask(world, layers, stage, stack, origin, program, call, fuel).await
}

#[allow(clippy::too_many_arguments)]
async fn resource<'a, E>(
    world: World<'a, E>,
    layers: Vec<&'a Overlay>,
    stage: &'a Stage,
    stack: &[ProgramId],
    origin: Origin,
    program: ProgramId,
    key: String,
    fuel: &mut Option<u64>,
) -> Result<std::result::Result<Vec<u8>, Refusal>>
where
    E: Context + Spawner,
{
    let call = ProgramCall::Resource(key);
    ask(world, layers, stage, stack, origin, program, call, fuel).await
}

#[allow(clippy::too_many_arguments)]
async fn ask<'a, E>(
    world: World<'a, E>,
    layers: Vec<&'a Overlay>,
    stage: &'a Stage,
    stack: &[ProgramId],
    origin: Origin,
    program: ProgramId,
    call: ProgramCall,
    fuel: &mut Option<u64>,
) -> Result<std::result::Result<Vec<u8>, Refusal>>
where
    E: Context + Spawner,
{
    let cycles = stack.contains(&program);
    if cycles {
        return Ok(Err(Refusal::new(
            reason::PROTOCOL,
            format!("{program} is already answering a query on this stack"),
        )));
    }
    let env = Env {
        network: world.network.to_vec(),
        height: world.height,
        time: world.time,
        me: program,
        origin,
        // a query acts as no one: it reads, and its origin says who asks
        sender: None,
        roles: world.roles.clone(),
        cause: Cause::Direct,
    };
    let mut stack = stack.to_vec();
    stack.push(env.me.clone());
    read(world, layers, stage, stack, env, call, fuel).await
}

pub async fn access<'a, E>(
    world: World<'a, E>,
    layers: Vec<&'a Overlay>,
    stage: &'a Stage,
    env: Env,
    payload: Vec<u8>,
    fuel: &mut Option<u64>,
) -> Result<std::result::Result<Vec<u8>, Refusal>>
where
    E: Context + Spawner,
{
    let stack = vec![env.me.clone()];
    read(
        world,
        layers,
        stage,
        stack,
        env,
        ProgramCall::Access(payload),
        fuel,
    )
    .await
}

async fn read<'a, E>(
    world: World<'a, E>,
    layers: Vec<&'a Overlay>,
    stage: &'a Stage,
    stack: Vec<ProgramId>,
    env: Env,
    call: ProgramCall,
    fuel: &mut Option<u64>,
) -> Result<std::result::Result<Vec<u8>, Refusal>>
where
    E: Context + Spawner,
{
    let Some(module) = world.loaded.module(&env.me) else {
        return Ok(Err(unknown(&env.me)));
    };
    let invocation = Invocation {
        env: env.clone(),
        call,
    };
    let mut unit = Query {
        world,
        env,
        layers,
        stage,
        stack,
        response: Vec::new(),
        fault: None,
    };
    let verdict = world
        .loaded
        .runtime()
        .run(module, invocation, &mut unit, fuel)
        .await;
    if let Some(fault) = unit.fault {
        return Err(fault);
    }
    Ok(match verdict {
        Ok(Ok(())) => Ok(unit.response),
        Ok(Err(refusal)) => Err(refusal),
        Err(fault) => Err(refusal_of(fault)),
    })
}

pub async fn held<'a, E>(
    world: World<'a, E>,
    layers: Vec<&'a Overlay>,
    stage: &'a Stage,
    stack: &[ProgramId],
    id: &BlobId,
    fuel: &mut Option<u64>,
) -> Result<bool>
where
    E: Context + Spawner,
{
    let asked = blob::contract::Query::Held { blob: *id };
    let answer = query(
        world,
        layers,
        stage,
        stack,
        Origin::System,
        crate::bound(world.roles, role::BLOBS)?.clone(),
        abi::encode(&asked),
        fuel,
    )
    .await?;
    let Ok(bytes) = answer else {
        return Ok(false);
    };
    let reply = abi::decode::<blob::contract::Reply>(&bytes);
    Ok(matches!(reply, Ok(blob::contract::Reply::Held(Some(_)))))
}

/// What the frame is charged for each message a run emits, on top of the
/// run it causes: the kernel's own work of dispatching it, which a message
/// refused before any run (an unknown target, too deep) costs too.
const EMIT_FUEL: u64 = 1_000;

pub struct Frame {
    pub fuel: Option<u64>,
    pub first_item: u64,
    pub next_item: u64,
}

pub struct Called {
    pub ticket: Ticket,
    pub target: ProgramId,
    pub payload: Vec<u8>,
    pub acting: Option<AccountNumber>,
}

pub struct Resumed {
    pub frame: FrameId,
    pub ticket: Ticket,
    pub answer: Answer,
}

#[derive(Default)]
pub struct Effects {
    pub emitted: Vec<(ItemRef, Message)>,
    pub calls: Vec<Called>,
    pub resumes: Vec<Resumed>,
    pub deferred: bool,
    pub parked: Option<Suspended>,
    /// Each owned key a guarded run wrote, once, in key order.
    pub writes: Vec<Write>,
}

impl Effects {
    /// Whether the run sent nothing: no message, call, resume, deferral or
    /// park.
    pub fn quiet(&self) -> bool {
        let sent = !self.emitted.is_empty() || !self.calls.is_empty() || !self.resumes.is_empty();
        !sent && !self.deferred && self.parked.is_none()
    }
}

/// Whether the ownership role guards what a run writes: an op, or an op
/// resumed, unless the kernel runs it, the validators' vote sent it, or it
/// is the ownership role's own. A network that binds no ownership role
/// guards nothing, so its runs note no write.
pub(crate) fn guarded(env: &Env, call: &ProgramCall) -> bool {
    let runs_an_op = matches!(call, ProgramCall::Execute(_) | ProgramCall::Resume { .. });
    let by_the_kernel = env.origin == Origin::System;
    let voted = match &env.origin {
        Origin::Program(program) => env.roles.get(role::VALIDATORS) == Some(program),
        Origin::External(_) | Origin::System => false,
    };
    let Some(guardian) = env.roles.get(role::OWNERSHIP) else {
        return false;
    };
    let guards = *guardian == env.me;
    runs_an_op && !by_the_kernel && !voted && !guards
}

struct Execute<'a, E>
where
    E: Context + Spawner,
{
    world: World<'a, E>,
    env: Env,
    overlay: &'a mut Overlay,
    stage: &'a mut Stage,
    frame: &'a mut Frame,
    events: Vec<Vec<u8>>,
    effects: Effects,
    output: Vec<u8>,
    fault: Option<Error>,
    guarded: bool,
    tables: &'a [Table],
    touched: BTreeMap<Vec<u8>, bool>,
}

impl<E> Execute<'_, E>
where
    E: Context + Spawner,
{
    /// Notes, the first time a guarded run touches an owned key, whether a
    /// value was there before the run.
    fn note(&mut self, key: &[u8]) -> Result<()> {
        let unwatched = !self.guarded || ownership::shared(self.tables, key);
        let noted = self.touched.contains_key(key);
        if unwatched || noted {
            return Ok(());
        }
        let view = View::new(self.world.store.storage(), vec![&*self.overlay]);
        let existed = view.get(&self.env.me, key)?.is_some();
        self.touched.insert(key.to_vec(), existed);
        Ok(())
    }

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

    /// Keeps the message for the frame to run once this handler returns.
    fn emit(&mut self, message: Message) -> HostReply {
        let item = ItemRef {
            source: self.env.me.clone(),
            item: self.frame.next_item,
        };
        self.frame.next_item += 1;
        self.effects.emitted.push((item.clone(), message));
        HostReply::Item(item)
    }

    fn call(
        &mut self,
        target: ProgramId,
        payload: Vec<u8>,
        acting: Option<AccountNumber>,
    ) -> Result<HostReply> {
        let ticket = parked::take_ticket(self.world.store.storage(), self.overlay)?;
        self.effects.calls.push(Called {
            ticket,
            target,
            payload,
            acting,
        });
        Ok(HostReply::Ticket(ticket))
    }

    fn defer(&mut self) -> HostReply {
        let Cause::Call { ticket } = self.env.cause else {
            return HostReply::Refused(Refusal::new(
                reason::WRONG_STATE,
                "this run answers no call",
            ));
        };
        self.effects.deferred = true;
        HostReply::Ticket(ticket)
    }

    fn resume(&mut self, frame: FrameId, ticket: Ticket, answer: Answer) -> Result<HostReply> {
        let from_the_role = self.world.roles.get(role::AWAITS) == Some(&self.env.me);
        if !from_the_role {
            return Ok(HostReply::Refused(Refusal::new(
                reason::UNAUTHORIZED,
                "only the awaits role resumes a frame",
            )));
        }
        let view = View::new(self.world.store.storage(), vec![&*self.overlay]);
        let parked = parked::record(&view, frame)?.is_some();
        let resuming = self
            .effects
            .resumes
            .iter()
            .any(|resumed| resumed.frame == frame);
        if !parked || resuming {
            return Ok(HostReply::Refused(Refusal::new(
                reason::NOT_FOUND,
                format!("frame {frame} is not parked"),
            )));
        }
        self.effects.resumes.push(Resumed {
            frame,
            ticket,
            answer,
        });
        Ok(HostReply::Done)
    }
}

#[async_trait::async_trait]
impl<E> runtime::Host for Execute<'_, E>
where
    E: Context + Spawner,
{
    async fn call(&mut self, op: HostOp, fuel: &mut Option<u64>) -> HostReply {
        let me = self.env.me.clone();
        let result = match op {
            HostOp::Set { key, value } => self.note(&key).map(|()| {
                self.overlay.set(&me, key, value);
                HostReply::Done
            }),
            HostOp::Delete(key) => self.note(&key).map(|()| {
                self.overlay.delete(&me, key);
                HostReply::Done
            }),
            HostOp::BlobPut { hash, kind, body } => Ok(self.put(hash, &kind, &body)),
            HostOp::Emit(message) => {
                *fuel = fuel.map(|left| left.saturating_sub(EMIT_FUEL));
                Ok(self.emit(message))
            }
            HostOp::Await {
                target,
                payload,
                acting,
            } => {
                *fuel = fuel.map(|left| left.saturating_sub(EMIT_FUEL));
                self.call(target, payload, acting)
            }
            HostOp::Defer => Ok(self.defer()),
            HostOp::Resume {
                frame,
                ticket,
                answer,
            } => self.resume(frame, ticket, answer),
            HostOp::Event(bytes) => {
                self.events.push(bytes);
                Ok(HostReply::Done)
            }
            HostOp::Output(bytes) => {
                self.output = bytes;
                Ok(HostReply::Done)
            }
            HostOp::Respond(_) => Ok(HostReply::Refused(Refusal::new(
                reason::UNSUPPORTED,
                "an op does not respond",
            ))),
            other => {
                let reader = Reader {
                    world: self.world,
                    layers: vec![&*self.overlay],
                    stage: &*self.stage,
                    env: &self.env,
                    stack: &[],
                };
                reader.serve(other, fuel).await
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
    frame: &mut Frame,
    program: &str,
    suspended: Option<Suspended>,
    invocation: Invocation,
) -> Result<(Receipt, Effects)>
where
    E: Context + Spawner,
{
    let Some(module) = world.loaded.module(program) else {
        return Ok((
            crate::rejected(program, unknown(program)),
            Effects::default(),
        ));
    };
    let checkpoint = overlay.checkpoint();
    let mut fuel = frame.fuel;
    let may_park = matches!(
        invocation.call,
        ProgramCall::Execute(_) | ProgramCall::Resume { .. }
    );
    let guarded = guarded(&invocation.env, &invocation.call);
    let tables = world.loaded.tables(program);
    let (verdict, events, mut effects, output, fault, touched) = {
        let mut unit = Execute {
            world,
            env: invocation.env.clone(),
            overlay: &mut *overlay,
            stage: &mut *stage,
            frame: &mut *frame,
            events: Vec::new(),
            effects: Effects::default(),
            output: Vec::new(),
            fault: None,
            guarded,
            tables,
            touched: BTreeMap::new(),
        };
        let runtime = world.loaded.runtime();
        let verdict = match (suspended, may_park) {
            (Some(suspended), _) => {
                runtime
                    .resume(suspended, invocation, &mut unit, &mut fuel)
                    .await
            }
            (None, true) => {
                runtime
                    .start(module, invocation, &mut unit, &mut fuel)
                    .await
            }
            (None, false) => runtime
                .run(module, invocation, &mut unit, &mut fuel)
                .await
                .map(Ran::Done),
        };
        (
            verdict,
            unit.events,
            unit.effects,
            unit.output,
            unit.fault,
            unit.touched,
        )
    };
    frame.fuel = fuel;
    if let Some(fault) = fault {
        return Err(fault);
    }
    let outcome = match verdict {
        Ok(Ran::Done(Ok(()))) => Outcome::Applied { output },
        Ok(Ran::Done(Err(refusal))) => Outcome::Rejected(refusal),
        Ok(Ran::Parked(suspended)) => {
            effects.parked = Some(suspended);
            Outcome::Applied { output }
        }
        Err(fault) => Outcome::Rejected(refusal_of(fault)),
    };
    let effects = match &outcome {
        Outcome::Applied { .. } => Effects {
            writes: written(world.store.storage(), overlay, program, tables, touched)?,
            ..effects
        },
        Outcome::Rejected(_) => {
            overlay.restore(checkpoint);
            discard_unrostered(world.store.storage(), overlay, stage)?;
            Effects::default()
        }
    };
    let receipt = Receipt {
        program: program.to_owned(),
        outcome,
        events,
        nested: Vec::new(),
    };
    Ok((receipt, effects))
}

/// The owned keys a run touched, as they stand once it returns: whether each
/// held a value before it and holds one after. A key that held none either
/// side changed nothing, and is left out.
fn written(
    storage: &Storage,
    overlay: &Overlay,
    program: &str,
    tables: &[Table],
    touched: BTreeMap<Vec<u8>, bool>,
) -> Result<Vec<Write>> {
    let view = View::new(storage, vec![overlay]);
    let mut writes = Vec::new();
    for (key, existed) in touched {
        let kept = view.get(program, &key)?.is_some();
        let changed = existed || kept;
        if !changed {
            continue;
        }
        let kind = ownership::kind_of(tables, &key);
        writes.push(Write {
            key,
            kind,
            existed,
            kept,
        });
    }
    Ok(writes)
}

pub(crate) fn discard_unrostered(
    storage: &Storage,
    overlay: &Overlay,
    stage: &mut Stage,
) -> Result<()> {
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
