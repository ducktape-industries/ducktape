use std::collections::BTreeMap;

use abi::{
    BlobId, Cause, Env, GuestCall, HostOp, HostReply, Invocation, ItemRef, Message, Origin,
    Outcome, ProgramId, Refusal, Roles, reason,
};
use blobs::{Blobs, Layered, Stage};
use commonware_runtime::Spawner;
use commonware_storage::Context;
use runtime::{Code, Fault, Limits, Runtime};
use state::{Overlay, Storage, Store, View};

use crate::{Error, Receipt, Result, crypto, namespace};

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

    /// Serves a read. A query it makes runs on `fuel`, the caller's.
    async fn serve(&self, op: HostOp, fuel: &mut Option<u64>) -> Result<HostReply> {
        let me = self.env.me.as_str();
        let reply = match op {
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
                    fuel,
                )
                .await?,
            ),
            HostOp::Crypto(op) => crypto::serve(op),
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
        network: world.network.to_vec(),
        height: world.height,
        time: world.time,
        me: program.clone(),
        origin,
        // a query acts as no one: it reads, and its origin says who asks
        sender: None,
        roles: world.roles.clone(),
        cause: Cause::Direct,
    };
    let mut stack = stack.to_vec();
    stack.push(program);
    let invocation = Invocation {
        env: env.clone(),
        call: GuestCall::Query(request),
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
        .run_within(module, invocation, &mut unit, fuel)
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

/// What the frame is charged for each message a run emits, on top of the
/// run it causes: the kernel's own work of dispatching it, which a message
/// refused before any run (an unknown target, too deep) costs too.
pub const EMIT_FUEL: u64 = 1_000;

/// What one submission's runs share: the fuel left of the network's limit,
/// and the number the next emitted message takes, so every item of the
/// frame is distinct whichever run emitted it.
pub struct Frame {
    pub fuel: Option<u64>,
    pub next_item: u64,
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
    emitted: Vec<(ItemRef, Message)>,
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

    /// Keeps the message for the frame to run once this handler returns.
    fn emit(&mut self, message: Message) -> HostReply {
        let item = ItemRef {
            source: self.env.me.clone(),
            item: self.frame.next_item,
        };
        self.frame.next_item += 1;
        self.emitted.push((item.clone(), message));
        HostReply::Item(item)
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
            HostOp::Set { key, value } => {
                self.overlay.set(&me, key, value);
                Ok(HostReply::Done)
            }
            HostOp::Delete(key) => {
                self.overlay.delete(&me, key);
                Ok(HostReply::Done)
            }
            HostOp::BlobPut { hash, kind, body } => Ok(self.put(hash, &kind, &body)),
            HostOp::Emit(message) => {
                *fuel = fuel.map(|left| left.saturating_sub(EMIT_FUEL));
                Ok(self.emit(message))
            }
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

/// One run of `program`: its receipt, and what it emitted (nothing when it
/// was rejected: its writes are undone here too).
pub async fn execute<E>(
    world: World<'_, E>,
    overlay: &mut Overlay,
    stage: &mut Stage,
    frame: &mut Frame,
    program: &str,
    invocation: Invocation,
) -> Result<(Receipt, Vec<(ItemRef, Message)>)>
where
    E: Context + Spawner,
{
    let Some(module) = world.loaded.module(program) else {
        return Ok((crate::rejected(program, unknown(program)), Vec::new()));
    };
    let checkpoint = overlay.checkpoint();
    let mut fuel = frame.fuel;
    let (verdict, events, emitted, output, fault) = {
        let mut unit = Execute {
            world,
            env: invocation.env.clone(),
            overlay: &mut *overlay,
            stage: &mut *stage,
            frame: &mut *frame,
            events: Vec::new(),
            emitted: Vec::new(),
            output: Vec::new(),
            fault: None,
        };
        let verdict = world
            .loaded
            .runtime()
            .run_within(module, invocation, &mut unit, &mut fuel)
            .await;
        (verdict, unit.events, unit.emitted, unit.output, unit.fault)
    };
    frame.fuel = fuel;
    if let Some(fault) = fault {
        return Err(fault);
    }
    let outcome = match verdict {
        Ok(Ok(())) => Outcome::Applied { output },
        Ok(Err(refusal)) => Outcome::Rejected(refusal),
        Err(fault) => Outcome::Rejected(refusal_of(fault)),
    };
    let emitted = match &outcome {
        Outcome::Applied { .. } => emitted,
        Outcome::Rejected(_) => {
            overlay.restore(checkpoint);
            discard_unrostered(world.store.storage(), overlay, stage)?;
            Vec::new()
        }
    };
    let receipt = Receipt {
        program: program.to_owned(),
        outcome,
        events,
        nested: Vec::new(),
    };
    Ok((receipt, emitted))
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

#[cfg(test)]
mod tests {
    use abi::{Principal, Roles};
    use commonware_runtime::{Runner as _, deterministic};
    use runtime::Host as _;

    use super::*;

    #[test]
    fn each_emitted_message_charges_the_frame() {
        deterministic::Runner::default().start(|context| async move {
            let dir = tempfile::tempdir().unwrap();
            let storage = Storage::open(&dir.path().join("state")).unwrap();
            let store = Store::open(context, "t", storage, Vec::new())
                .await
                .unwrap();
            let blobs = Blobs::open(&dir.path().join("blobs")).unwrap();
            let loaded = Loaded::new(Limits::default());
            let roles = Roles {
                registry: "registry".into(),
                validators: "validators".into(),
                identity: "identity".into(),
            };
            let world = World {
                store: &store,
                blobs: &blobs,
                loaded: &loaded,
                network: b"net",
                roles: &roles,
                height: 1,
                time: 1,
            };
            let env = Env {
                network: b"net".to_vec(),
                height: 1,
                time: 1,
                me: "ping".into(),
                origin: Origin::System,
                sender: Some(Principal::System),
                roles: roles.clone(),
                cause: Cause::Direct,
            };
            let mut overlay = Overlay::default();
            let mut stage = Stage::default();
            let mut frame = Frame {
                fuel: None,
                next_item: 0,
            };
            let mut unit = Execute {
                world,
                env,
                overlay: &mut overlay,
                stage: &mut stage,
                frame: &mut frame,
                events: Vec::new(),
                emitted: Vec::new(),
                output: Vec::new(),
                fault: None,
            };
            const K: u64 = 5;
            let start = 100_000;
            let mut fuel = Some(start);
            for _ in 0..K {
                let message = Message {
                    target: "pong".into(),
                    payload: Vec::new(),
                    reply: false,
                };
                unit.call(HostOp::Emit(message), &mut fuel).await;
            }
            assert_eq!(unit.emitted.len(), K as usize);
            assert_eq!(fuel, Some(start - K * EMIT_FUEL));

            // an emit the run cannot pay for leaves it nothing
            let mut fuel = Some(EMIT_FUEL - 1);
            let message = Message {
                target: "pong".into(),
                payload: Vec::new(),
                reply: false,
            };
            unit.call(HostOp::Emit(message), &mut fuel).await;
            assert_eq!(fuel, Some(0));
        });
    }
}
