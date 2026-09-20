pub use abi;
pub use abi::{
    Blob, BlobHeader, BlobId, Cause, CryptoOp, CryptoReply, Entry, Env, GuestCall, GuestReply,
    HashKind, HostOp, HostReply, ItemRef, Message, Origin, Outcome, ProgramId, Refusal, Root, Scan,
    Scheme, reason,
};

pub trait Program {
    fn init(_params: &[u8]) -> Result<(), Refusal> {
        Ok(())
    }
    fn execute(payload: &[u8]) -> Result<(), Refusal>;
    fn query(request: &[u8]) -> Result<(), Refusal>;
}

pub fn dispatch<P: Program>(call: GuestCall) -> GuestReply {
    match call {
        GuestCall::Init(params) => P::init(&params),
        GuestCall::Execute(payload) => P::execute(&payload),
        GuestCall::Query(request) => P::query(&request),
    }
}

#[macro_export]
macro_rules! program {
    ($program:ty) => {
        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn alloc(len: u32) -> u32 {
            $crate::exports::alloc(len)
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn call(ptr: u32, len: u32) -> u64 {
            $crate::exports::call::<$program>(ptr, len)
        }
    };
}

pub mod exports {
    use super::{GuestCall, GuestReply, Program, Refusal, reason};

    pub fn alloc(len: u32) -> u32 {
        let mut buffer = Vec::<u8>::with_capacity(len as usize);
        let ptr = buffer.as_mut_ptr();
        core::mem::forget(buffer);
        ptr as u32
    }

    pub fn call<P: Program>(ptr: u32, len: u32) -> u64 {
        let request = unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) };
        let reply: GuestReply = match abi::decode::<GuestCall>(&request) {
            Ok(call) => super::dispatch::<P>(call),
            Err(fault) => Err(Refusal::new(reason::PROTOCOL, fault.sentence)),
        };
        leak(abi::encode(&reply))
    }

    pub fn leak(bytes: Vec<u8>) -> u64 {
        let len = bytes.len() as u64;
        let ptr = bytes.as_ptr() as u64;
        core::mem::forget(bytes);
        (ptr << 32) | len
    }
}

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "ducktape")]
    unsafe extern "C" {
        pub fn host_call(ptr: u32, len: u32) -> u32;
        pub fn host_take(ptr: u32);
    }
}

#[cfg(target_arch = "wasm32")]
pub fn host(op: &HostOp) -> HostReply {
    let request = abi::encode(op);
    let len = unsafe { imports::host_call(request.as_ptr() as u32, request.len() as u32) };
    let mut reply = vec![0u8; len as usize];
    unsafe { imports::host_take(reply.as_mut_ptr() as u32) };
    match abi::decode(&reply) {
        Ok(reply) => reply,
        Err(fault) => panic!("{fault}"),
    }
}

#[cfg(target_arch = "wasm32")]
pub mod host_ops {
    use super::*;

    fn protocol(expected: &str, got: HostReply) -> ! {
        panic!("host answered {got:?} where {expected} was expected")
    }

    pub fn env() -> Env {
        match host(&HostOp::Env) {
            HostReply::Env(env) => env,
            other => protocol("env", other),
        }
    }

    pub fn get(key: impl AsRef<[u8]>) -> Option<Vec<u8>> {
        match host(&HostOp::Get(key.as_ref().to_vec())) {
            HostReply::Value(value) => value,
            other => protocol("value", other),
        }
    }

    pub fn set(key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) {
        match host(&HostOp::Set {
            key: key.into(),
            value: value.into(),
        }) {
            HostReply::Done => {}
            other => protocol("done", other),
        }
    }

    pub fn delete(key: impl Into<Vec<u8>>) {
        match host(&HostOp::Delete(key.into())) {
            HostReply::Done => {}
            other => protocol("done", other),
        }
    }

    pub fn scan(scan: Scan) -> Vec<Entry> {
        match host(&HostOp::Scan(scan)) {
            HostReply::Entries(entries) => entries,
            other => protocol("entries", other),
        }
    }

    pub fn committed_get(key: impl AsRef<[u8]>) -> Option<Vec<u8>> {
        match host(&HostOp::CommittedGet(key.as_ref().to_vec())) {
            HostReply::Value(value) => value,
            other => protocol("value", other),
        }
    }

    pub fn committed_scan(scan: Scan) -> Vec<Entry> {
        match host(&HostOp::CommittedScan(scan)) {
            HostReply::Entries(entries) => entries,
            other => protocol("entries", other),
        }
    }

    pub fn blob_put(
        hash: HashKind,
        kind: impl Into<String>,
        body: impl Into<Vec<u8>>,
    ) -> Result<BlobId, Refusal> {
        match host(&HostOp::BlobPut {
            hash,
            kind: kind.into(),
            body: body.into(),
        }) {
            HostReply::BlobId(id) => Ok(id),
            HostReply::Refused(refusal) => Err(refusal),
            other => protocol("blob id", other),
        }
    }

    pub fn blob_get(id: BlobId) -> Option<Blob> {
        match host(&HostOp::BlobGet(id)) {
            HostReply::Blob(blob) => blob,
            other => protocol("blob", other),
        }
    }

    pub fn blob_stat(id: BlobId) -> Option<BlobHeader> {
        match host(&HostOp::BlobStat(id)) {
            HostReply::BlobHeader(header) => header,
            other => protocol("blob header", other),
        }
    }

    pub fn blob_read(id: BlobId, offset: u64, len: u64) -> Option<Vec<u8>> {
        match host(&HostOp::BlobRead { id, offset, len }) {
            HostReply::Value(bytes) => bytes,
            other => protocol("value", other),
        }
    }

    pub fn root(program: impl Into<ProgramId>) -> Option<Root> {
        match host(&HostOp::Root(program.into())) {
            HostReply::Root(root) => root,
            other => protocol("root", other),
        }
    }

    pub fn query(
        program: impl Into<ProgramId>,
        request: impl Into<Vec<u8>>,
    ) -> Result<Vec<u8>, Refusal> {
        match host(&HostOp::Query {
            program: program.into(),
            request: request.into(),
        }) {
            HostReply::Query(answer) => answer,
            other => protocol("query answer", other),
        }
    }

    pub fn emit(target: impl Into<ProgramId>, payload: impl Into<Vec<u8>>) -> ItemRef {
        send(target, payload, false)
    }

    pub fn call(target: impl Into<ProgramId>, payload: impl Into<Vec<u8>>) -> ItemRef {
        send(target, payload, true)
    }

    fn send(target: impl Into<ProgramId>, payload: impl Into<Vec<u8>>, reply: bool) -> ItemRef {
        match host(&HostOp::Emit(Message {
            target: target.into(),
            payload: payload.into(),
            reply,
        })) {
            HostReply::Item(item) => item,
            other => protocol("item", other),
        }
    }

    pub fn event(payload: impl Into<Vec<u8>>) {
        match host(&HostOp::Event(payload.into())) {
            HostReply::Done => {}
            other => protocol("done", other),
        }
    }

    pub fn output(bytes: impl Into<Vec<u8>>) {
        match host(&HostOp::Output(bytes.into())) {
            HostReply::Done => {}
            other => protocol("done", other),
        }
    }

    pub fn respond(bytes: impl Into<Vec<u8>>) {
        match host(&HostOp::Respond(bytes.into())) {
            HostReply::Done => {}
            other => protocol("done", other),
        }
    }

    pub fn sha256(bytes: impl Into<Vec<u8>>) -> [u8; 32] {
        match host(&HostOp::Crypto(CryptoOp::Sha256(bytes.into()))) {
            HostReply::Crypto(CryptoReply::Digest(digest)) => digest,
            other => protocol("digest", other),
        }
    }

    pub fn verify(
        scheme: Scheme,
        key: impl Into<Vec<u8>>,
        message: impl Into<Vec<u8>>,
        signature: impl Into<Vec<u8>>,
    ) -> Result<bool, Refusal> {
        match host(&HostOp::Crypto(CryptoOp::Verify {
            scheme,
            key: key.into(),
            message: message.into(),
            signature: signature.into(),
        })) {
            HostReply::Crypto(CryptoReply::Verified(valid)) => Ok(valid),
            HostReply::Refused(refusal) => Err(refusal),
            other => protocol("verdict", other),
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use host_ops::*;
