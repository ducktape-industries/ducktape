pub use abi;
pub use abi::{
    Blob, BlobHeader, BlobId, Cause, CryptoOp, CryptoReply, Entry, Env, GuestCall, GuestReply,
    HashKind, HostOp, HostReply, Invocation, ItemRef, Message, Origin, Outcome, ProgramId, Refusal,
    Root, Scan, Scheme, reason,
};

#[cfg(target_arch = "wasm32")]
pub use context::{Execute, Program, Query, Reads, dispatch};

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

#[cfg(target_arch = "wasm32")]
pub mod exports {
    use super::{Invocation, Program, Refusal, dispatch, reason};

    pub fn alloc(len: u32) -> u32 {
        let mut buffer = Vec::<u8>::with_capacity(len as usize);
        let ptr = buffer.as_mut_ptr();
        core::mem::forget(buffer);
        ptr as u32
    }

    pub fn call<P: Program>(ptr: u32, len: u32) -> u64 {
        let request = unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) };
        let reply = match abi::decode::<Invocation>(&request) {
            Ok(invocation) => dispatch::<P>(invocation),
            Err(fault) => Err(Refusal::new(reason::PROTOCOL, fault.sentence)),
        };
        leak(abi::encode(&reply))
    }

    fn leak(bytes: Vec<u8>) -> u64 {
        let len = bytes.len() as u64;
        let ptr = bytes.as_ptr() as u64;
        core::mem::forget(bytes);
        (ptr << 32) | len
    }
}

#[cfg(target_arch = "wasm32")]
mod context {
    use borsh::{BorshDeserialize, BorshSerialize};

    use super::*;

    #[link(wasm_import_module = "ducktape")]
    unsafe extern "C" {
        fn host_call(ptr: u32, len: u32) -> u32;
        fn host_take(ptr: u32);
    }

    fn host(op: &HostOp) -> HostReply {
        let request = abi::encode(op);
        let len = unsafe { host_call(request.as_ptr() as u32, request.len() as u32) };
        let mut reply = vec![0u8; len as usize];
        unsafe { host_take(reply.as_mut_ptr() as u32) };
        match abi::decode(&reply) {
            Ok(reply) => reply,
            Err(fault) => panic!("{fault}"),
        }
    }

    fn protocol(expected: &str, got: HostReply) -> ! {
        panic!("host answered {got:?} where {expected} was expected")
    }

    pub trait Program {
        fn init(_ctx: &mut Execute, _env: &Env, _params: &[u8]) -> Result<(), Refusal> {
            Ok(())
        }
        fn execute(ctx: &mut Execute, env: &Env, payload: &[u8]) -> Result<(), Refusal>;
        fn query(ctx: &mut Query, env: &Env, request: &[u8]) -> Result<(), Refusal>;
    }

    pub fn dispatch<P: Program>(invocation: Invocation) -> GuestReply {
        let Invocation { env, call } = invocation;
        match call {
            GuestCall::Init(params) => P::init(&mut Execute(()), &env, &params),
            GuestCall::Execute(payload) => P::execute(&mut Execute(()), &env, &payload),
            GuestCall::Query(request) => P::query(&mut Query(()), &env, &request),
        }
    }

    pub trait Reads {
        fn host(&self, op: &HostOp) -> HostReply;

        fn get(&self, key: impl AsRef<[u8]>) -> Option<Vec<u8>> {
            match self.host(&HostOp::Get(key.as_ref().to_vec())) {
                HostReply::Value(value) => value,
                other => protocol("value", other),
            }
        }

        fn scan(&self, scan: Scan) -> Vec<Entry> {
            match self.host(&HostOp::Scan(scan)) {
                HostReply::Entries(entries) => entries,
                other => protocol("entries", other),
            }
        }

        fn committed_get(&self, key: impl AsRef<[u8]>) -> Option<Vec<u8>> {
            match self.host(&HostOp::CommittedGet(key.as_ref().to_vec())) {
                HostReply::Value(value) => value,
                other => protocol("value", other),
            }
        }

        fn committed_scan(&self, scan: Scan) -> Vec<Entry> {
            match self.host(&HostOp::CommittedScan(scan)) {
                HostReply::Entries(entries) => entries,
                other => protocol("entries", other),
            }
        }

        fn record<T: BorshDeserialize>(&self, key: impl AsRef<[u8]>) -> Result<Option<T>, Refusal> {
            self.get(key).map(|bytes| abi::decode(&bytes)).transpose()
        }

        fn records<T: BorshDeserialize>(&self, scan: Scan) -> Result<Vec<(Vec<u8>, T)>, Refusal> {
            self.scan(scan)
                .into_iter()
                .map(|entry| Ok((entry.key, abi::decode(&entry.value)?)))
                .collect()
        }

        fn blob_get(&self, id: BlobId) -> Option<Blob> {
            match self.host(&HostOp::BlobGet(id)) {
                HostReply::Blob(blob) => blob,
                other => protocol("blob", other),
            }
        }

        fn blob_stat(&self, id: BlobId) -> Option<BlobHeader> {
            match self.host(&HostOp::BlobStat(id)) {
                HostReply::BlobHeader(header) => header,
                other => protocol("blob header", other),
            }
        }

        fn blob_read(&self, id: BlobId, offset: u64, len: u64) -> Option<Vec<u8>> {
            match self.host(&HostOp::BlobRead { id, offset, len }) {
                HostReply::Value(bytes) => bytes,
                other => protocol("value", other),
            }
        }

        fn root(&self, program: impl Into<ProgramId>) -> Option<Root> {
            match self.host(&HostOp::Root(program.into())) {
                HostReply::Root(root) => root,
                other => protocol("root", other),
            }
        }

        fn query(
            &self,
            program: impl Into<ProgramId>,
            request: impl Into<Vec<u8>>,
        ) -> Result<Vec<u8>, Refusal> {
            match self.host(&HostOp::Query {
                program: program.into(),
                request: request.into(),
            }) {
                HostReply::Query(answer) => answer,
                other => protocol("query answer", other),
            }
        }

        fn ask<Q: BorshSerialize, R: BorshDeserialize>(
            &self,
            program: impl Into<ProgramId>,
            request: &Q,
        ) -> Result<R, Refusal> {
            abi::decode(&self.query(program, abi::encode(request))?)
        }

        fn sha256(&self, bytes: impl Into<Vec<u8>>) -> [u8; 32] {
            match self.host(&HostOp::Crypto(CryptoOp::Sha256(bytes.into()))) {
                HostReply::Crypto(CryptoReply::Digest(digest)) => digest,
                other => protocol("digest", other),
            }
        }

        fn verify(
            &self,
            scheme: Scheme,
            key: impl Into<Vec<u8>>,
            namespace: impl Into<Vec<u8>>,
            message: impl Into<Vec<u8>>,
            signature: impl Into<Vec<u8>>,
        ) -> Result<bool, Refusal> {
            match self.host(&HostOp::Crypto(CryptoOp::Verify {
                scheme,
                key: key.into(),
                namespace: namespace.into(),
                message: message.into(),
                signature: signature.into(),
            })) {
                HostReply::Crypto(CryptoReply::Verified(valid)) => Ok(valid),
                HostReply::Refused(refusal) => Err(refusal),
                other => protocol("verdict", other),
            }
        }
    }

    pub struct Execute(());

    impl Reads for Execute {
        fn host(&self, op: &HostOp) -> HostReply {
            host(op)
        }
    }

    impl Execute {
        pub fn set(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) {
            match host(&HostOp::Set {
                key: key.into(),
                value: value.into(),
            }) {
                HostReply::Done => {}
                other => protocol("done", other),
            }
        }

        pub fn delete(&mut self, key: impl Into<Vec<u8>>) {
            match host(&HostOp::Delete(key.into())) {
                HostReply::Done => {}
                other => protocol("done", other),
            }
        }

        pub fn put<T: BorshSerialize>(&mut self, key: impl Into<Vec<u8>>, record: &T) {
            self.set(key, abi::encode(record))
        }

        pub fn blob_put(
            &mut self,
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

        pub fn emit(
            &mut self,
            target: impl Into<ProgramId>,
            payload: impl Into<Vec<u8>>,
        ) -> ItemRef {
            self.send(target, payload, false)
        }

        pub fn call(
            &mut self,
            target: impl Into<ProgramId>,
            payload: impl Into<Vec<u8>>,
        ) -> ItemRef {
            self.send(target, payload, true)
        }

        fn send(
            &mut self,
            target: impl Into<ProgramId>,
            payload: impl Into<Vec<u8>>,
            reply: bool,
        ) -> ItemRef {
            match host(&HostOp::Emit(Message {
                target: target.into(),
                payload: payload.into(),
                reply,
            })) {
                HostReply::Item(item) => item,
                other => protocol("item", other),
            }
        }

        pub fn event(&mut self, payload: impl Into<Vec<u8>>) {
            match host(&HostOp::Event(payload.into())) {
                HostReply::Done => {}
                other => protocol("done", other),
            }
        }

        pub fn output(&mut self, bytes: impl Into<Vec<u8>>) {
            match host(&HostOp::Output(bytes.into())) {
                HostReply::Done => {}
                other => protocol("done", other),
            }
        }
    }

    pub struct Query(());

    impl Reads for Query {
        fn host(&self, op: &HostOp) -> HostReply {
            host(op)
        }
    }

    impl Query {
        pub fn respond(&mut self, bytes: impl Into<Vec<u8>>) {
            match host(&HostOp::Respond(bytes.into())) {
                HostReply::Done => {}
                other => protocol("done", other),
            }
        }

        pub fn reply<R: BorshSerialize>(&mut self, reply: &R) {
            self.respond(abi::encode(reply))
        }
    }
}
