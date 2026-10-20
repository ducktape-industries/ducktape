mod image;
mod stacks;

use std::future::Future;
use std::sync::Arc;

use abi::{HostOp, HostReply, Invocation, ProgramReply};
use borsh::{BorshDeserialize, BorshSerialize};
use futures::future::{Either, select};
use tokio::sync::{OnceCell, mpsc, oneshot};
use wasmtime::{
    Caller, Config, Engine, InstancePre, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, WasmBacktraceDetails,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Limits {
    pub fuel: Option<u64>,
    pub memory_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    Load(String),
    Trap(String),
    Protocol(String),
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Fault::Load(sentence) => write!(f, "program does not load: {sentence}"),
            Fault::Trap(sentence) => write!(f, "program trapped: {sentence}"),
            Fault::Protocol(sentence) => write!(f, "program broke the contract: {sentence}"),
        }
    }
}

impl std::error::Error for Fault {}

/// The sentence of the trap a run takes when its budget runs out, whether
/// its own instructions spent it or the host charged it away.
pub const OUT_OF_FUEL: &str = "the run spent its fuel";

#[async_trait::async_trait]
pub trait Host {
    /// Serves `op`. `fuel` is what the calling run has left (`None` when
    /// the network meters none); what the host leaves there is what the run
    /// resumes with, so the host charges its work to the run by lowering it.
    async fn call(&mut self, op: HostOp, fuel: &mut Option<u64>) -> HostReply;
}

pub use image::{Codec, Image, PACK_LEVEL, PAGE_BYTES, Packed, Page};

#[derive(Clone)]
pub struct Code {
    module: Module,
    linked: Option<InstancePre<Data>>,
    fresh: Arc<OnceCell<Vec<u8>>>,
}

pub enum Ran {
    Done(ProgramReply),
    Parked(Suspended),
}

pub struct Suspended {
    code: Code,
    store: Store<Data>,
    inbox: mpsc::UnboundedReceiver<Request>,
    instance: wasmtime::Instance,
    memory: Memory,
}

impl core::fmt::Debug for Suspended {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let bytes = self.memory.data_size(&self.store);
        write!(f, "Suspended({bytes} bytes)")
    }
}

#[derive(Clone)]
pub struct Runtime {
    engine: Engine,
    limits: Limits,
}

type Request = (
    HostOp,
    Option<u64>,
    oneshot::Sender<(HostReply, Option<u64>)>,
);

struct Data {
    requests: mpsc::UnboundedSender<Request>,
    pending: Vec<u8>,
    limiter: StoreLimits,
    parked: bool,
}

impl Runtime {
    pub fn new(limits: Limits) -> Runtime {
        let mut config = Config::new();
        config
            .consume_fuel(limits.fuel.is_some())
            // a trap's sentence reaches a reply run's `Completion`, so state:
            // no backtrace is captured, nor read from the node's
            // `WASMTIME_BACKTRACE_DETAILS`
            .wasm_backtrace_max_frames(None)
            .wasm_backtrace_details(WasmBacktraceDetails::Disable)
            .cranelift_nan_canonicalization(true)
            .wasm_simd(false)
            .wasm_relaxed_simd(false)
            .wasm_threads(false)
            .wasm_gc(false)
            .wasm_tail_call(false)
            .wasm_multi_memory(false)
            .with_host_stack(Arc::new(stacks::Stacks::default()));
        Runtime {
            engine: Engine::new(&config).expect("a fixed wasmtime configuration"),
            limits,
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn load(&self, bytes: &[u8]) -> Result<Code, Fault> {
        let module = Module::new(&self.engine, bytes).map_err(load)?;
        let linked = linker(&self.engine)
            .ok()
            .and_then(|linker| linker.instantiate_pre(&module).ok());
        Ok(Code {
            module,
            linked,
            fresh: Arc::new(OnceCell::new()),
        })
    }

    /// Runs `invocation` on a budget: the run starts with `fuel` (`None`
    /// when the network meters none) and leaves what it did not burn there,
    /// so the runs of one frame share one budget.
    pub async fn run(
        &self,
        code: &Code,
        invocation: Invocation,
        host: &mut (impl Host + ?Sized),
        fuel: &mut Option<u64>,
    ) -> Result<ProgramReply, Fault> {
        match self.start(code, invocation, host, fuel).await? {
            Ran::Done(reply) => Ok(reply),
            Ran::Parked(_) => Err(Fault::Protocol(
                "the program parked in a run that must finish".into(),
            )),
        }
    }

    pub async fn start(
        &self,
        code: &Code,
        invocation: Invocation,
        host: &mut (impl Host + ?Sized),
        fuel: &mut Option<u64>,
    ) -> Result<Ran, Fault> {
        let suspended = self.instantiate(code, fuel).await?;
        self.resume(suspended, invocation, host, fuel).await
    }

    pub async fn resume(
        &self,
        mut suspended: Suspended,
        invocation: Invocation,
        host: &mut (impl Host + ?Sized),
        fuel: &mut Option<u64>,
    ) -> Result<Ran, Fault> {
        let reply = drive(&mut suspended, invocation, host, fuel).await?;
        let parked = suspended.store.data().parked;
        match (parked, reply) {
            (false, reply) => Ok(Ran::Done(reply)),
            (true, Ok(())) => Ok(Ran::Parked(suspended)),
            (true, Err(refusal)) => Err(Fault::Protocol(format!(
                "the program parked and refused: {}",
                refusal.sentence
            ))),
        }
    }

    pub async fn freeze(&self, suspended: &Suspended) -> Result<Image, Fault> {
        let fresh = self.fresh(&suspended.code).await?;
        Ok(Image::of(fresh, suspended.memory.data(&suspended.store)))
    }

    pub async fn thaw(
        &self,
        code: &Code,
        image: &Image,
        fuel: &mut Option<u64>,
    ) -> Result<Suspended, Fault> {
        let memory = image.onto(self.fresh(code).await?);
        let mut suspended = self.instantiate(code, fuel).await?;
        restore(&mut suspended.store, suspended.memory, &memory)?;
        Ok(suspended)
    }

    async fn fresh<'c>(&self, code: &'c Code) -> Result<&'c [u8], Fault> {
        let fresh = code
            .fresh
            .get_or_try_init(|| async {
                let suspended = self.instantiate(code, &mut None).await?;
                Ok(suspended.memory.data(&suspended.store).to_vec())
            })
            .await?;
        Ok(fresh)
    }

    async fn instantiate(&self, code: &Code, fuel: &mut Option<u64>) -> Result<Suspended, Fault> {
        if *fuel == Some(0) {
            return Err(Fault::Trap(OUT_OF_FUEL.into()));
        }
        let (requests, inbox) = mpsc::unbounded_channel();
        let mut builder = StoreLimitsBuilder::new();
        if let Some(bytes) = self.limits.memory_bytes {
            builder = builder.memory_size(bytes as usize);
        }
        let mut store = Store::new(
            &self.engine,
            Data {
                requests,
                pending: Vec::new(),
                limiter: builder.build(),
                parked: false,
            },
        );
        store.limiter(|data| &mut data.limiter);
        if let Some(fuel) = *fuel {
            store.set_fuel(fuel).map_err(load)?;
        }
        let instance = match &code.linked {
            Some(linked) => linked.instantiate_async(&mut store).await.map_err(load),
            None => linker(&self.engine)?
                .instantiate_async(&mut store, &code.module)
                .await
                .map_err(load),
        };
        if fuel.is_some() {
            *fuel = Some(store.get_fuel().map_err(load)?);
        }
        let instance = instance?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| Fault::Load("program exports no memory".into()))?;
        Ok(Suspended {
            code: code.clone(),
            store,
            inbox,
            instance,
            memory,
        })
    }
}

async fn drive(
    suspended: &mut Suspended,
    invocation: Invocation,
    host: &mut (impl Host + ?Sized),
    fuel: &mut Option<u64>,
) -> Result<ProgramReply, Fault> {
    if *fuel == Some(0) {
        return Err(Fault::Trap(OUT_OF_FUEL.into()));
    }
    if let Some(fuel) = *fuel {
        suspended.store.set_fuel(fuel).map_err(load)?;
    }
    suspended.store.data_mut().parked = false;
    let Suspended {
        store,
        inbox,
        instance,
        memory,
        ..
    } = suspended;
    let verdict = {
        let mut program = Box::pin(call(store, *instance, *memory, invocation));
        loop {
            let request = Box::pin(inbox.recv());
            match select(program, request).await {
                Either::Left((verdict, _)) => break verdict,
                Either::Right((Some((op, mut left, reply_to)), resumed)) => {
                    program = resumed;
                    let reply = host.call(op, &mut left).await;
                    let _ = reply_to.send((reply, left));
                }
                Either::Right((None, _)) => {
                    unreachable!("the store holds the request sender for the whole run")
                }
            }
        }
    };
    if fuel.is_some() {
        *fuel = Some(store.get_fuel().map_err(load)?);
    }
    verdict
}

fn linker(engine: &Engine) -> Result<Linker<Data>, Fault> {
    let mut linker = Linker::new(engine);
    linker
        .func_wrap_async("ducktape", "host_call", host_call)
        .and_then(|linker| linker.func_wrap("ducktape", "host_take", host_take))
        .and_then(|linker| linker.func_wrap("ducktape", "host_park", host_park))
        .map_err(load)?;
    Ok(linker)
}

fn host_park(mut caller: Caller<'_, Data>) {
    caller.data_mut().parked = true;
}

async fn call(
    store: &mut Store<Data>,
    instance: wasmtime::Instance,
    memory: Memory,
    invocation: Invocation,
) -> Result<ProgramReply, Fault> {
    let alloc = instance
        .get_typed_func::<u32, u32>(&mut *store, "alloc")
        .map_err(load)?;
    let entry = instance
        .get_typed_func::<(u32, u32), u64>(&mut *store, "call")
        .map_err(load)?;
    let request = abi::encode(&invocation);
    let ptr = alloc
        .call_async(&mut *store, request.len() as u32)
        .await
        .map_err(trap)?;
    memory
        .write(&mut *store, ptr as usize, &request)
        .map_err(|e| Fault::Protocol(format!("alloc handed back {ptr}: {e}")))?;
    let packed = entry
        .call_async(&mut *store, (ptr, request.len() as u32))
        .await
        .map_err(trap)?;
    let reply = read(&memory, &*store, (packed >> 32) as u32, packed as u32)
        .map_err(|e| Fault::Protocol(format!("reply out of memory: {e}")))?;
    abi::decode::<ProgramReply>(&reply).map_err(|r| Fault::Protocol(r.sentence))
}

const WASM_PAGE_BYTES: usize = 64 << 10;

fn restore(store: &mut Store<Data>, memory: Memory, restored: &[u8]) -> Result<(), Fault> {
    let pages = restored.len().div_ceil(WASM_PAGE_BYTES) as u64;
    let have = memory.size(&*store);
    if pages > have {
        memory.grow(&mut *store, pages - have).map_err(load)?;
    }
    memory
        .write(&mut *store, 0, restored)
        .map_err(|e| Fault::Protocol(format!("the image does not fit its memory: {e}")))
}

fn load(error: wasmtime::Error) -> Fault {
    Fault::Load(format!("{error:#}"))
}

/// A trap's sentence is the kernel's, keyed on the trap kind alone: it can
/// reach state, so it never carries wasmtime's own text, which changes
/// across wasmtime versions.
fn trap(error: wasmtime::Error) -> Fault {
    use wasmtime::Trap;
    let sentence = match error.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => OUT_OF_FUEL,
        Some(Trap::UnreachableCodeReached) => "the run trapped: unreachable",
        Some(Trap::MemoryOutOfBounds) => "the run trapped: out of bounds memory access",
        Some(Trap::IntegerDivisionByZero) => "the run trapped: integer divide by zero",
        Some(Trap::IntegerOverflow) => "the run trapped: integer overflow",
        Some(Trap::BadConversionToInteger) => "the run trapped: invalid conversion to integer",
        Some(Trap::StackOverflow) => "the run trapped: call stack exhausted",
        Some(Trap::TableOutOfBounds) => "the run trapped: out of bounds table access",
        Some(Trap::IndirectCallToNull) => "the run trapped: uninitialized table element",
        Some(Trap::BadSignature) => "the run trapped: indirect call type mismatch",
        _ => "the run trapped",
    };
    Fault::Trap(sentence.into())
}

fn host_call<'a>(
    mut caller: Caller<'a, Data>,
    (ptr, len): (u32, u32),
) -> Box<dyn Future<Output = wasmtime::Result<u32>> + Send + 'a> {
    Box::new(async move {
        let memory = memory_of(&mut caller)?;
        let request = read(&memory, &caller, ptr, len)?;
        let op: HostOp = abi::decode(&request).map_err(|r| wasmtime::Error::msg(r.sentence))?;
        let (reply_to, reply) = oneshot::channel();
        // an unmetered store has no fuel to read: the host sees `None`
        let fuel = caller.get_fuel().ok();
        caller
            .data()
            .requests
            .send((op, fuel, reply_to))
            .map_err(|_| wasmtime::Error::msg("the kernel stopped listening"))?;
        let (reply, fuel) = reply
            .await
            .map_err(|_| wasmtime::Error::msg("the kernel stopped answering"))?;
        if let Some(fuel) = fuel {
            caller.set_fuel(fuel)?;
        }
        let data = caller.data_mut();
        data.pending = abi::encode(&reply);
        Ok(data.pending.len() as u32)
    })
}

fn host_take(mut caller: Caller<'_, Data>, ptr: u32) -> wasmtime::Result<()> {
    let pending = core::mem::take(&mut caller.data_mut().pending);
    let memory = memory_of(&mut caller)?;
    memory.write(&mut caller, ptr as usize, &pending)?;
    Ok(())
}

fn memory_of(caller: &mut Caller<'_, Data>) -> wasmtime::Result<Memory> {
    caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("program exports no memory"))
}

fn read(
    memory: &Memory,
    store: impl wasmtime::AsContext,
    ptr: u32,
    len: u32,
) -> wasmtime::Result<Vec<u8>> {
    let mut bytes = vec![0u8; len as usize];
    memory.read(store, ptr as usize, &mut bytes)?;
    Ok(bytes)
}
