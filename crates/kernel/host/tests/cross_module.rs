//! proves typed cross-module composition end-to-end through the host: a greeter
//! module QUERIES a directory module and, from the result, WRITES a greeting to
//! both directory and kv — using only the modules' interface crates. runs the
//! directory both NATIVE and as the `directory` guest component (the first real
//! wasm tenant): the composing peer cannot tell which runtime serves it.

use commonware_runtime::{Runner as _, deterministic};
use directory::Directory;
use directory::{DirMsg, DirQuery, DirReply, decode_reply, encode_msg, encode_query};
use greeter::Greeter;
use host::Host;
use sdk::{Ctx, Error, Module, Msg, StateRoot};
use statesync::qmdb::QmdbStore;
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `directory` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const DIRECTORY_WASM: &[u8] = include_bytes!("fixtures/directory.component.wasm");

struct QueryCycler {
    id: &'static str,
    next: &'static str,
}

#[async_trait::async_trait(?Send)]
impl Module for QueryCycler {
    fn id(&self) -> String {
        self.id.into()
    }

    fn root(&self) -> StateRoot {
        StateRoot::ZERO
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }

    async fn query_with(&self, ctx: &dyn Ctx, _req: &[u8]) -> Result<Vec<u8>, Error> {
        ctx.query(self.next, b"cycle").await
    }
}

#[test]
fn greeter_reads_directory_and_writes_a_derived_greeting() {
    greet_through(|| Box::new(Directory::new("directory")));
}

/// the same composition with the directory served by the WASM runtime: the
/// native greeter's `ctx.query` routes into the component (memoized-replay
/// sibling read on the greeter side is not needed — the WASM side here is the
/// QUERIED one), and the typed `DirMsg::Set` follow-up executes in the guest.
#[test]
fn greeter_composes_with_a_wasm_directory() {
    greet_through(|| {
        Box::new(WasmModule::from_bytes("directory", DIRECTORY_WASM).expect("load component"))
    });
}

fn greet_through(directory: impl Fn() -> Box<dyn Module> + Send + 'static) {
    deterministic::Runner::default().start(|context| async move {
        let kv = kv::Kv::new("kv", Box::new(QmdbStore::init(context, "kv").await));
        let mut host = Host::genesis(vec![
            Box::new(kv),
            directory(),
            Box::new(Greeter::new("greeter")),
        ])
        .expect("genesis");

        // seed the directory, then trigger the greeter.
        host.submit(Msg {
            target: "directory".into(),
            payload: encode_msg(&DirMsg::Set {
                key: "name".into(),
                value: "world".into(),
            }),
        })
        .await
        .expect("seed");

        let kv_root_before = host.module_root("kv").unwrap();

        host.submit(Msg {
            target: "greeter".into(),
            payload: b"name".to_vec(),
        })
        .await
        .expect("greet");

        // the greeting is DERIVED from the cross-module query (name=world), not hardcoded.
        let reply = host
            .query(
                "directory",
                &encode_query(&DirQuery::Get {
                    key: "greeting:name".into(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            decode_reply(&reply).unwrap(),
            DirReply::Value(Some("hello world".into()))
        );

        // the greeting is also readable from the QMDB kv module via a real async query.
        let kvr = host
            .query(
                "kv",
                &kv::encode_query(&kv::KvQuery::Get {
                    key: b"greeting:name".to_vec(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            kv::decode_reply(&kvr).unwrap(),
            kv::KvReply::Value(Some(b"hello world".to_vec()))
        );

        // and the typed follow-up reached the qmdb kv module too (its real root moved).
        assert_ne!(
            host.module_root("kv").unwrap(),
            kv_root_before,
            "greeter's kv write must land"
        );
    });
}

#[test]
fn genesis_rejects_duplicate_ids() {
    deterministic::Runner::default().start(|_| async move {
        let a = Directory::new("dup");
        let b = Directory::new("dup");
        let err = Host::genesis(vec![Box::new(a), Box::new(b)]);
        assert!(
            err.is_err(),
            "duplicate module id must be rejected at genesis"
        );
    });
}

#[test]
fn query_cycles_are_rejected() {
    deterministic::Runner::default().start(|_| async move {
        let host = Host::genesis(vec![
            Box::new(QueryCycler { id: "a", next: "b" }),
            Box::new(QueryCycler { id: "b", next: "a" }),
        ])
        .expect("genesis");

        let err = host
            .query("a", b"start")
            .await
            .expect_err("query cycle must fail instead of recursing");

        assert!(
            matches!(err, Error::Module { ref reason, ref sentence } if reason == "query_cycle" && sentence == "query cycle: a")
        );
    });
}
