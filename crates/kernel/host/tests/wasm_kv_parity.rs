//! the STORE-BACKED cutover-continuity proof for kv: the `kv` guest component
//! over `WasmModule::with_store(QmdbStore)` and the native `Kv` over the same
//! store shape are ROOT-CONTINUOUS — the same op sequence commits the
//! IDENTICAL qmdb merkle root after every block (both roots ARE the store's
//! root). kv carries no genesis config and reads no env, so the whole
//! equivalence surface is ops in, roots + `Get` replies out.
//!
//! the rejection matrix pins the WRITE-TIME size caps inside the compiled
//! component: an over-cap key/value must reject identically on both runtimes
//! — the caps are the poison-pill guard, so a runtime that let one through
//! would commit a value every later read panics on.

use commonware_runtime::{Runner as _, Supervisor as _, deterministic};
use host::{BlockContext, Host, SubmitError};
use kv::{Kv, KvMsg, KvQuery, MAX_KEY_LEN, MAX_VALUE_LEN, encode, encode_query};
use sdk::{Error, Msg, Origin, StateRoot};
use statesync::qmdb::QmdbStore;
use wasm_host::WasmModule;

/// GENERATED artifact — built from the `kv` module's guest port by
/// guest-builder (`make wasm-modules`); committed so this proof is self-contained.
const KV_WASM: &[u8] = include_bytes!("fixtures/kv.component.wasm");

/// a fresh qmdb store. `label` doubles as the store id (the deterministic
/// runtime keys storage partitions by id alone).
async fn kv_store(
    context: &deterministic::Context,
    label: &'static str,
) -> QmdbStore<deterministic::Context> {
    QmdbStore::init(context.child(label), label).await
}

fn wasm_kv(store: Box<dyn sdk::MerkleStore>) -> WasmModule {
    WasmModule::with_store("kv", KV_WASM, store).expect("load component")
}

async fn native_host(context: &deterministic::Context) -> Host {
    let store = kv_store(context, "native_kv").await;
    Host::genesis(vec![Box::new(Kv::new("kv", Box::new(store)))]).expect("genesis")
}

async fn wasm_host_(context: &deterministic::Context) -> Host {
    let store = kv_store(context, "wasm_kv").await;
    Host::genesis(vec![Box::new(wasm_kv(Box::new(store)))]).expect("genesis")
}

fn set(key: &[u8], value: &[u8]) -> Msg {
    Msg {
        target: "kv".into(),
        payload: encode(&KvMsg::Set {
            key: key.to_vec(),
            value: value.to_vec(),
        }),
    }
}

fn get(key: &[u8]) -> Vec<u8> {
    encode_query(&KvQuery::Get { key: key.to_vec() })
}

/// one block's agreed context: both runtimes must see the identical env
/// (kv's execute reads none of it, which is itself part of the parity claim).
fn block(height: u64) -> BlockContext {
    BlockContext {
        height,
        consensus_time: 1_000 + height,
        origin: Origin::External(vec![0xA1; 32]),
    }
}

fn root_of(h: &Host) -> StateRoot {
    h.module_root("kv").expect("kv registered")
}

async fn replies(h: &Host) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for key in [b"k1".as_ref(), b"k2", "Ω-unicode".as_bytes(), b"absent"] {
        out.push(h.query("kv", &get(key)).await.expect("kv query"));
    }
    out
}

#[test]
fn same_ops_same_values_roots_in_lockstep_and_continuous() {
    deterministic::Runner::default().start(|context| async move {
        same_ops_inner(&context).await;
    });
}

async fn same_ops_inner(context: &deterministic::Context) {
    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;

    // ROOT-CONTINUITY from GENESIS: both roots are the (empty) store's merkle
    // root, identical across the runtimes.
    assert_eq!(
        root_of(&native),
        root_of(&wasm),
        "genesis roots must be continuous across the runtimes"
    );

    // writes, an overwrite, a unicode key, and an at-cap value — every block
    // moves the root on BOTH sides and the roots stay identical.
    let ops = [
        set(b"k1", b"v1"),
        set(b"k2", b"v2"),
        set(b"k1", b"overwritten"),
        set("Ω-unicode".as_bytes(), b"value"),
        set(&vec![b'k'; MAX_KEY_LEN], &vec![0u8; MAX_VALUE_LEN]),
    ];
    for (height, op) in ops.into_iter().enumerate() {
        let height = height as u64 + 1;
        let (n_before, w_before) = (root_of(&native), root_of(&wasm));
        native
            .submit_at(block(height), op.clone())
            .await
            .expect("native submit");
        wasm.submit_at(block(height), op)
            .await
            .expect("wasm submit");
        assert_ne!(root_of(&native), n_before, "native root stuck at {height}");
        assert_ne!(root_of(&wasm), w_before, "wasm root stuck at {height}");
        assert_eq!(
            root_of(&native),
            root_of(&wasm),
            "the two runtimes diverged at {height}"
        );
        assert_eq!(
            replies(&native).await,
            replies(&wasm).await,
            "kv replies diverge after block {height}"
        );
    }
}

#[test]
fn over_cap_writes_reject_identically_and_leave_no_trace() {
    deterministic::Runner::default().start(|context| async move {
        rejections_inner(&context).await;
    });
}

async fn rejections_inner(context: &deterministic::Context) {
    let mut native = native_host(context).await;
    let mut wasm = wasm_host_(context).await;

    let rejects: Vec<(Msg, &str)> = vec![
        (set(&vec![b'k'; MAX_KEY_LEN + 1], b"v"), "key too large"),
        (set(b"k", &vec![0u8; MAX_VALUE_LEN + 1]), "value too large"),
        (
            Msg {
                target: "kv".into(),
                payload: b"definitely-not-json".to_vec(),
            },
            "expected value",
        ),
    ];
    for (i, (msg, needle)) in rejects.into_iter().enumerate() {
        let height = i as u64 + 1;
        let (n_before, w_before) = (root_of(&native), root_of(&wasm));
        let n_err = native
            .submit_at(block(height), msg.clone())
            .await
            .expect_err("native must reject");
        let w_err = wasm
            .submit_at(block(height), msg)
            .await
            .expect_err("wasm must reject");
        // both reject DETERMINISTICALLY with the native module's reason (the
        // wasm side wraps it in its wit-error rendering — containment, not
        // string equality).
        let SubmitError::Rejected(Error::Module {
            reason: n_reason,
            sentence: n_msg,
        }) = n_err
        else {
            panic!("native rejection shape: {n_err:?}");
        };
        let SubmitError::Rejected(Error::Module {
            reason: w_reason,
            sentence: w_msg,
        }) = w_err
        else {
            panic!("wasm rejection shape: {w_err:?}");
        };
        assert!(n_msg.contains(needle), "native reason: {n_msg}");
        assert_eq!(n_reason, w_reason, "wasm token must match the native token");
        assert!(
            w_msg.contains(needle),
            "wasm reason must carry the native reason: {w_msg}"
        );
        assert_eq!(root_of(&native), n_before, "native root moved on reject");
        assert_eq!(root_of(&wasm), w_before, "wasm root moved on reject");
        assert_eq!(replies(&native).await, replies(&wasm).await);
    }
}
