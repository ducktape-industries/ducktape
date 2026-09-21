use std::future::Future;

use abi::{GuestReply, HostOp, HostReply, Invocation};
use borsh::{BorshDeserialize, BorshSerialize};
use futures::future::{Either, select};
use tokio::sync::{mpsc, oneshot};
use wasmtime::{
    Caller, Config, Engine, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
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

#[async_trait::async_trait]
pub trait Host {
    async fn call(&mut self, op: HostOp) -> HostReply;
}

#[derive(Clone)]
pub struct Code {
    module: Module,
}

pub struct Runtime {
    engine: Engine,
    limits: Limits,
}

type Request = (HostOp, oneshot::Sender<HostReply>);

struct Data {
    requests: mpsc::UnboundedSender<Request>,
    pending: Vec<u8>,
    limiter: StoreLimits,
}

impl Runtime {
    pub fn new(limits: Limits) -> Runtime {
        let mut config = Config::new();
        config
            .consume_fuel(limits.fuel.is_some())
            .cranelift_nan_canonicalization(true)
            .wasm_simd(false)
            .wasm_relaxed_simd(false)
            .wasm_threads(false)
            .wasm_gc(false)
            .wasm_tail_call(false)
            .wasm_multi_memory(false);
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
        Ok(Code { module })
    }

    pub async fn run(
        &self,
        code: &Code,
        invocation: Invocation,
        host: &mut (impl Host + ?Sized),
    ) -> Result<GuestReply, Fault> {
        let (requests, mut inbox) = mpsc::unbounded_channel();
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
            },
        );
        store.limiter(|data| &mut data.limiter);
        if let Some(fuel) = self.limits.fuel {
            store.set_fuel(fuel).map_err(load)?;
        }
        let mut guest = Box::pin(drive(&self.engine, &mut store, &code.module, invocation));
        loop {
            let request = Box::pin(inbox.recv());
            match select(guest, request).await {
                Either::Left((verdict, _)) => return verdict,
                Either::Right((Some((op, reply_to)), resumed)) => {
                    guest = resumed;
                    let _ = reply_to.send(host.call(op).await);
                }
                Either::Right((None, _)) => {
                    unreachable!("the store holds the request sender for the whole run")
                }
            }
        }
    }
}

async fn drive(
    engine: &Engine,
    store: &mut Store<Data>,
    module: &Module,
    invocation: Invocation,
) -> Result<GuestReply, Fault> {
    let mut linker = Linker::new(engine);
    linker
        .func_wrap_async("ducktape", "host_call", host_call)
        .and_then(|linker| linker.func_wrap("ducktape", "host_take", host_take))
        .map_err(load)?;
    let instance = linker
        .instantiate_async(&mut *store, module)
        .await
        .map_err(load)?;
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| Fault::Load("program exports no memory".into()))?;
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
    abi::decode::<GuestReply>(&reply).map_err(|r| Fault::Protocol(r.sentence))
}

fn load(error: wasmtime::Error) -> Fault {
    Fault::Load(format!("{error:#}"))
}

fn trap(error: wasmtime::Error) -> Fault {
    Fault::Trap(format!("{error:#}"))
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
        caller
            .data()
            .requests
            .send((op, reply_to))
            .map_err(|_| wasmtime::Error::msg("the kernel stopped listening"))?;
        let reply = reply
            .await
            .map_err(|_| wasmtime::Error::msg("the kernel stopped answering"))?;
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
