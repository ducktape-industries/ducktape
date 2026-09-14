//! L2/L3 proof: a wasm module dispatches through the `sdk::Module` seam exactly
//! like a native one — staged writes, block-boundary commit, deterministic root,
//! and a hot-swap that keeps the host-owned state.
//!
//! The fixture is a GENERATED artifact (built from `crates/guests/hello-wasm`
//! by the module build target); it is committed so this proof is self-contained.

use sdk::{Env, Module, Msg, Origin, StateRoot};
use wasm_host::{Backing, Shape, WasmModule};

const HELLO: &[u8] = include_bytes!("fixtures/hello.component.wasm");

use sdk_testkit::TestCtx;

fn mock(me: &str) -> TestCtx {
    TestCtx::with_env(Env {
        height: 1,
        consensus_time: 0,
        origin: Origin::System,
        me: me.into(),
        cause: sdk::Cause::Direct,
    })
}

async fn inc(m: &mut WasmModule, ctx: &mut TestCtx) {
    m.execute(
        ctx,
        &Msg {
            target: "hello".into(),
            payload: b"inc".to_vec(),
        },
    )
    .await
    .expect("inc executes");
}

fn count(bytes: Vec<u8>) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("8-byte count"))
}

#[tokio::test]
async fn dispatch_commit_query_roundtrip() {
    let mut m = WasmModule::from_bytes("hello", HELLO).expect("load");
    let mut ctx = mock("hello");
    let root0 = m.root();

    inc(&mut m, &mut ctx).await;
    inc(&mut m, &mut ctx).await;
    inc(&mut m, &mut ctx).await;

    // staged, not committed → root still reflects committed (empty) state.
    assert_eq!(m.root(), root0, "root reflects committed state only");

    m.commit_block().await.expect("commit");
    assert_ne!(m.root(), root0, "commit advances the root");

    assert_eq!(count(m.query(b"").await.expect("query")), 3);
    assert_eq!(ctx.events().len(), 3, "one event per inc, drained to ctx");
    assert_eq!(ctx.events()[2].source, "hello");
}

#[tokio::test]
async fn abort_discards_staged() {
    let mut m = WasmModule::from_bytes("hello", HELLO).expect("load");
    let mut ctx = mock("hello");
    let root0 = m.root();
    inc(&mut m, &mut ctx).await;
    inc(&mut m, &mut ctx).await;
    m.abort_block().await.expect("abort");
    assert_eq!(m.root(), root0, "abort leaves no trace");
    assert_eq!(count(m.query(b"").await.expect("query")), 0);
}

#[tokio::test]
async fn deterministic_root_across_instances() {
    async fn run() -> StateRoot {
        let mut m = WasmModule::from_bytes("hello", HELLO).expect("load");
        let mut ctx = mock("hello");
        for _ in 0..5 {
            inc(&mut m, &mut ctx).await;
        }
        m.commit_block().await.expect("commit");
        m.root()
    }
    assert_eq!(
        run().await,
        run().await,
        "same ops → identical root on independent instances"
    );
}

#[tokio::test]
async fn snapshot_install_round_trip() {
    let mut src = WasmModule::from_bytes("hello", HELLO).expect("load");
    let mut ctx = mock("hello");
    inc(&mut src, &mut ctx).await;
    inc(&mut src, &mut ctx).await;
    src.commit_block().await.expect("commit");
    let root = src.root();

    // sha256(snapshot()) IS the root — the checkpoint ships the exact preimage.
    let bytes = src.snapshot();
    let mut dst = WasmModule::from_bytes("hello", HELLO).expect("load");
    dst.install(&bytes, root).expect("verify-then-adopt");
    assert_eq!(dst.root(), root, "installed root equals the source root");
    assert_eq!(count(dst.query(b"").await.expect("query")), 2);

    // tampered / truncated / trailing bytes are refused, target untouched.
    let mut flipped = bytes.clone();
    let mid = flipped.len() / 2;
    flipped[mid] ^= 0x01;
    assert!(dst.install(&flipped, root).is_err());
    assert!(dst.install(&bytes[..bytes.len() - 1], root).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(dst.install(&trailing, root).is_err());
    assert_eq!(dst.root(), root, "failed installs left the target untouched");
}

#[tokio::test]
async fn hot_swap_keeps_state() {
    let mut m = WasmModule::from_bytes("hello", HELLO).expect("load");
    let mut ctx = mock("hello");
    inc(&mut m, &mut ctx).await;
    inc(&mut m, &mut ctx).await;
    m.commit_block().await.expect("commit");
    let before = m.query(b"").await.expect("query");
    assert_eq!(count(before.clone()), 2);

    // swap to the SAME code (stand-in for a new version); state must survive.
    m.swap_code(&module_artifact::Artifact::module(HELLO.to_vec()).encode())
        .expect("swap");
    assert_eq!(
        m.query(b"").await.expect("query"),
        before,
        "swap keeps state"
    );

    // the swapped code still executes against the kept store.
    inc(&mut m, &mut ctx).await;
    m.commit_block().await.expect("commit");
    assert_eq!(count(m.query(b"").await.expect("query")), 3);
}

/// A CODE SWAP NEVER CHANGES A MODULE'S SHAPE: the state layout is the swap
/// contract, so a replacement declaring another backing cannot keep this
/// state — refused by name at the boundary, and the running code and state
/// are left exactly as they were.
#[tokio::test]
async fn a_swap_to_another_backing_is_refused_and_keeps_the_running_code() {
    const OBJECT: &[u8] = include_bytes!("fixtures/object.component.wasm");
    let mut m = WasmModule::from_bytes("hello", HELLO).expect("load");
    let mut ctx = mock("hello");
    inc(&mut m, &mut ctx).await;
    m.commit_block().await.expect("commit");
    let before = m.root();

    let err = m
        .swap_code(&module_artifact::Artifact::module(OBJECT.to_vec()).encode())
        .expect_err("an odb-declared replacement over a map is refused");
    assert!(
        err.to_string().contains("declares a Odb backing"),
        "the refusal names the declared backing: {err}"
    );
    assert_eq!(m.root(), before, "the refused swap moved nothing");
    inc(&mut m, &mut ctx).await;
    m.commit_block().await.expect("commit");
    assert_eq!(count(m.query(b"").await.expect("query")), 2, "the running code still runs");
}

/// THE READINESS PROBE MUST ACTUALLY LOAD. A validator signals `SwapReady` off
/// the declared-shape read, and byte residency alone would let a node on an
/// older binary arm a swap it then rejects every op to — so reading the shape
/// runs the real compile + instantiate, not merely a look at the bytes.
#[test]
fn declared_shape_reads_a_real_component_and_refuses_garbage() {
    let shape = WasmModule::declared_shape(HELLO).expect("the shipped fixture loads on this binary");
    assert_eq!(
        shape,
        Shape {
            backing: Backing::Map,
            config: Vec::new(),
            committed_queries: false,
        },
        "hello declares a map-backed, unconfigured, read-your-writes shape"
    );

    for (what, bytes) in [
        ("empty", Vec::new()),
        ("not wasm at all", b"this is not a component".to_vec()),
        // a valid wasm preamble with a truncated body: passes the magic
        // number, dies in validation — the shape a half-fetched blob has.
        ("truncated", b"\0asm\x0d\0\x01\0\x01\x02".to_vec()),
    ] {
        assert!(
            WasmModule::declared_shape(&bytes).is_err(),
            "{what} bytes must never read as a loadable component"
        );
    }
}

/// THE ABI QUESTION IS NOT THE LOADABILITY QUESTION. `speaks_module_abi` asks
/// only whether the bytes are a `ducktape:module` at all, so a module boundary
/// can SKIP code committed for another plane (the `ducktape:netstack` guest,
/// whose world exports configure/step/snapshot/restore) instead of failing
/// closed on it forever — while a genuine module keeps its fail-closed
/// treatment whatever this build makes of it.
#[test]
fn speaks_module_abi_separates_a_module_from_another_plane_s_component() {
    const NETSTACK: &[u8] = include_bytes!("../../../networking/netstack-machine/component.wasm");

    assert!(
        wasm_host::speaks_module_abi(HELLO),
        "the shipped module fixture is a `ducktape:module`"
    );
    assert!(
        !wasm_host::speaks_module_abi(NETSTACK),
        "the netstack guest is a component, but not a module"
    );
    assert!(
        WasmModule::declared_shape(NETSTACK).is_err(),
        "and it is not loadable as one either — the two answers differ only for a MODULE"
    );
    for (what, bytes) in [
        ("empty", Vec::new()),
        ("not wasm at all", b"this is not a component".to_vec()),
        ("truncated", b"\0asm\x0d\0\x01\0\x01\x02".to_vec()),
    ] {
        assert!(
            !wasm_host::speaks_module_abi(&bytes),
            "{what} bytes are no component at all"
        );
    }
}

/// A MODULE RUNS OVER THE SUBSTRATE ITS CODE DECLARES, NEVER ANOTHER: the host
/// wrapping a map-declared component over a store is a wiring bug refused by
/// name at load, not a module silently computing a root over the wrong
/// substrate.
#[tokio::test]
async fn a_backing_the_component_did_not_declare_is_refused() {
    let store = sdk_testkit::MemStore::default();
    let err = WasmModule::with_store("hello", HELLO, Box::new(store))
        .err()
        .expect("a map-declared component over a store is refused");
    assert!(
        err.to_string().contains("declares a Map backing"),
        "the refusal names the declared backing: {err}"
    );
    WasmModule::from_bytes("hello", HELLO).expect("the declared backing loads");
}
