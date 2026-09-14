//! The live-code-update proof: a REAL wasm module (`hello`) and the REAL code
//! registry (`modreg`) on one host, driven across a height-gated swap boundary
//! to a SECOND component (`hello-replacement`, which `inc`s by 100 instead of 1).
//!
//! What must hold at the boundary `H`:
//!   * the host's out-of-block `realize_module_swaps(H, src)` fetches the
//!     committed target hash's bytes, verifies sha256, and swaps the component
//!     KEEPING the host-owned state — the module's `root()` stays identical,
//!     while the global root changes to bind the new deployment;
//!   * the drain injects EXACTLY ONE System-origin modreg `Advance` in block `H`,
//!     flipping the committed active hash into the root-hash (the consensus
//!     commitment to the new code);
//!   * block `H`'s ops execute the NEW logic over the KEPT state (2 + 100, never
//!     102-from-zero and never 3);
//!   * a node lacking the bytes (or holding tampered bytes) FAILS CLOSED;
//!   * a state-sync joiner — post-activation committed state, NO pending swap
//!     left to arm — still reconciles its genesis code to the committed ACTIVE
//!     hash instead of forking on stale code.
//!
//! Fixtures are GENERATED artifacts (see `crates/guests/hello-wasm` and
//! `crates/guests/hello-wasm-replacement`),
//! committed so the proof is self-contained.

use std::collections::BTreeMap;

use futures::executor::block_on;
use sha2::Digest;

use host::{BlockContext, CodeSource, Host, MODULES_ID};
use modules::{Modules, ModulesMsg, ModulesQuery, ModulesReply};
use sdk::{Error, Msg, Origin, StateRoot};
use wasm_host::WasmModule;

const HELLO_V1: &[u8] = include_bytes!("fixtures/hello.component.wasm");
const HELLO_REPLACEMENT: &[u8] = include_bytes!("fixtures/hello-replacement.component.wasm");

/// the swap boundary used throughout: far enough past genesis to clear
/// `modules::MIN_SWAP_LEAD` from the scheduling block.
const H: u64 = 10;

fn deployment(bytes: &[u8]) -> Vec<u8> {
    module_artifact::Artifact::module(bytes.to_vec()).encode()
}

fn sha(bytes: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(deployment(bytes)).to_vec()
}

/// the test-side `CodeSource`: a plain in-memory content-addressed map — the
/// node injects a blobstore-backed one, the proof injects this.
struct MapSource(BTreeMap<Vec<u8>, Vec<u8>>);

impl MapSource {
    fn with(components: &[&[u8]]) -> Self {
        Self(components.iter().map(|c| (sha(c), deployment(c))).collect())
    }
}

#[async_trait::async_trait(?Send)]
impl CodeSource for MapSource {
    async fn fetch(&self, code_hash: &[u8]) -> Option<Vec<u8>> {
        self.0.get(code_hash).cloned()
    }

    fn origin(&self) -> &'static str {
        "test_map"
    }
}

/// the one validator key the readiness gate counts in these proofs.
const MEMBER: [u8; 32] = [7; 32];

/// a host with the code registry, a real valset (one member — the readiness
/// denominator), and the wasm `hello` module (running v1), with v1 registered
/// as hello's genesis-active code.
fn host_with_wasm() -> Host {
    let mut host = Host::new();
    host.register(Box::new(Modules::new(
        MODULES_ID,
        Box::new(sdk_testkit::MemStore::new()),
        "valset",
        "governance",
    )));
    let mut valset = valset::Valset::new(
        "valset",
        Box::new(sdk_testkit::MemStore::new()),
        "governance",
    );
    block_on(valset.seed(MEMBER.to_vec())).expect("seed valset");
    block_on(valset.finish_seed()).expect("seed valset");
    host.register(Box::new(valset));
    host.register(Box::new(
        WasmModule::from_bytes("hello", HELLO_V1).expect("load v1"),
    ));
    submit(
        &mut host,
        0,
        Origin::System,
        modules_msg(&ModulesMsg::RegisterModule {
            module_id: "hello".into(),
            kind: modules::Kind::Module,
            code_hash: sha(HELLO_V1),
        }),
    );
    host
}

fn submit(host: &mut Host, height: u64, origin: Origin, msg: Msg) {
    let ctx = BlockContext {
        height,
        consensus_time: height,
        origin,
    };
    block_on(host.submit_at(ctx, msg)).expect("block applies");
}

fn modules_msg(m: &ModulesMsg) -> Msg {
    Msg {
        target: MODULES_ID.into(),
        payload: modules::encode_msg(m),
    }
}

fn schedule_msg(activation_height: u64, code_hash: Vec<u8>) -> Msg {
    modules_msg(&ModulesMsg::ScheduleSwap {
        name: "hello-replacement".into(),
        module_id: "hello".into(),
        activation_height,
        code_hash,
    })
}

/// the member's byte-receipt signal: what `code_announce` self-submits once
/// the component is verified-resident. latches the swap `ready` (R = n = 1).
fn signal_ready_msg() -> Msg {
    modules_msg(&ModulesMsg::SwapReady {
        name: "hello-replacement".into(),
        module_id: "hello".into(),
        code_hash: sha(HELLO_REPLACEMENT),
    })
}

fn inc_msg() -> Msg {
    Msg {
        target: "hello".into(),
        payload: b"inc".to_vec(),
    }
}

fn count(host: &Host) -> u64 {
    let bytes = block_on(host.query("hello", b"")).expect("count query");
    u64::from_le_bytes(bytes.try_into().expect("8-byte count"))
}

fn active_hash(host: &Host) -> (Vec<u8>, bool) {
    let req = modules::encode_query(&ModulesQuery::ModuleStatus);
    let bytes = block_on(host.query(MODULES_ID, &req)).expect("status");
    match modules::decode_reply(&bytes).expect("decode") {
        ModulesReply::ModuleStatus { modules } => {
            let m = modules
                .iter()
                .find(|m| m.module_id == "hello")
                .expect("hello entry");
            (m.active_code_hash.clone(), m.pending.is_some())
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

fn realize(host: &mut Host, height: u64, src: &dyn CodeSource) -> Result<(), Error> {
    block_on(host.realize_module_swaps(height, src))
}

/// drive the WHOLE swap scenario and return the final root-hash — shared by the
/// headline proof and the cross-node determinism check.
fn run_swap_scenario() -> (Host, StateRoot) {
    let mut host = host_with_wasm();
    let src = MapSource::with(&[HELLO_V1, HELLO_REPLACEMENT]);

    // two v1 incs: count 2, one step each.
    submit(&mut host, 1, Origin::External(vec![7; 32]), inc_msg());
    submit(&mut host, 2, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(count(&host), 2, "v1 steps by 1");

    // governance-shaped schedule: swap hello -> replacement at H.
    submit(
        &mut host,
        3,
        Origin::System,
        schedule_msg(H, sha(HELLO_REPLACEMENT)),
    );
    // the (sole) member verified the bytes and signals — the swap latches
    // ready; from here activation is the height floor alone.
    submit(
        &mut host,
        4,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );

    // below H nothing arms: realization is a no-op on the running code.
    realize(&mut host, H - 1, &src).expect("below H is Ok");
    submit(&mut host, H - 1, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(count(&host), 3, "still v1 below the boundary");

    // THE BOUNDARY. realize first (block H must execute the new code), then the
    // block: its root op runs replacement logic and the drain's injected modreg Advance
    // flips the committed active hash in the same block.
    let wasm_root_before = host.module_root("hello").expect("hello root");
    let root_hash_before = host.root_hash();
    realize(&mut host, H, &src).expect("swap realizes at H");
    assert_eq!(
        host.module_root("hello").expect("hello root"),
        wasm_root_before,
        "the swap keeps the host-owned state: root is byte-identical"
    );
    assert_ne!(
        host.root_hash(),
        root_hash_before,
        "the global root binds the running deployment as well as its state"
    );
    // idempotent: a second realization at the same height is a no-op.
    realize(&mut host, H, &src).expect("re-realize is Ok");

    submit(&mut host, H, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(
        count(&host),
        103,
        "replacement logic (+100) over KEPT state (3)"
    );
    let (active, pending) = active_hash(&host);
    assert_eq!(
        active,
        sha(HELLO_REPLACEMENT),
        "Advance flipped the committed hash at H"
    );
    assert!(!pending, "the pending slot is freed at H");

    // after the boundary: reconciliation stays a no-op, replacement keeps running.
    realize(&mut host, H + 1, &src).expect("post-H realize is Ok");
    submit(&mut host, H + 1, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(count(&host), 203, "replacement keeps stepping by 100");

    let final_hash = host.root_hash();
    (host, final_hash)
}

/// the headline proof: new code goes LIVE at `H` over kept state, the committed
/// active hash flips in the same block, and the swap itself never moves a root.
#[test]
fn live_swap_at_boundary_keeps_state_and_runs_new_code() {
    run_swap_scenario();
}

/// two independent nodes running the identical finalized sequence land on the
/// identical root-hash — the swap realization introduces no per-node divergence.
#[test]
fn deterministic_across_nodes() {
    let (_, a) = run_swap_scenario();
    let (_, b) = run_swap_scenario();
    assert_eq!(a, b, "same sequence, same root-hash on both nodes");
}

/// FAIL-CLOSED: a node that lacks the armed hash's bytes — or holds bytes that
/// do not match it — must refuse the boundary, leaving the running code untouched.
#[test]
fn fails_closed_on_missing_or_tampered_bytes() {
    let mut host = host_with_wasm();
    submit(
        &mut host,
        3,
        Origin::System,
        schedule_msg(H, sha(HELLO_REPLACEMENT)),
    );
    submit(
        &mut host,
        4,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );

    // missing bytes: the source only has v1 (this node SIGNALED honestly in
    // consensus but its local store lost the bytes — the boundary still
    // refuses rather than forking).
    let missing = MapSource::with(&[HELLO_V1]);
    let err = realize(&mut host, H, &missing).expect_err("absent bytes fail closed");
    assert!(matches!(err, Error::Module(m) if m.contains("absent")));

    // tampered bytes: active bytes filed under the replacement hash — sha mismatch.
    let tampered = MapSource(BTreeMap::from([(
        sha(HELLO_REPLACEMENT),
        HELLO_V1.to_vec(),
    )]));
    let err = realize(&mut host, H, &tampered).expect_err("mismatched bytes fail closed");
    assert!(matches!(err, Error::Module(m) if m.contains("do not match")));

    // neither attempt swapped anything: the module still runs v1.
    submit(&mut host, H, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(count(&host), 1, "still v1 after both refused boundaries");
}

/// the state-sync joiner: it installs POST-activation committed registry state —
/// active hash already points at the replacement, NO pending swap left to arm — while its genesis wiring
/// loaded v1. reconciliation must read the committed ACTIVE hash (not just armed
/// pendings) and bring the running code to the replacement, or the joiner forks on stale code.
#[test]
fn statesync_joiner_reconciles_to_committed_active_hash() {
    // source node: run the full swap so its committed registry is post-activation.
    let (source, _) = run_swap_scenario();
    let (active, pending) = active_hash(&source);
    assert_eq!(active, sha(HELLO_REPLACEMENT));
    assert!(!pending, "post-activation: nothing pending");

    // joiner: modreg rebuilt to the source's committed state (the real
    // joiner pulls the qmdb op range — `modules/tests/sync_round_trip.rs`
    // pins that wire; here the deterministic REPLAY of the same admin ops
    // reconstructs the identical record set over the MemStore double, and
    // the roots must agree), wasm module freshly wired from GENESIS (v1)
    // code.
    let modreg_root = source.module_root(MODULES_ID).expect("modreg root");
    let mut joiner = Host::new();
    joiner.register(Box::new(Modules::new(
        MODULES_ID,
        Box::new(sdk_testkit::MemStore::new()),
        "valset",
        "governance",
    )));
    let mut joiner_valset = valset::Valset::new(
        "valset",
        Box::new(sdk_testkit::MemStore::new()),
        "governance",
    );
    block_on(joiner_valset.seed(MEMBER.to_vec())).expect("seed valset");
    block_on(joiner_valset.finish_seed()).expect("seed valset");
    joiner.register(Box::new(joiner_valset));
    joiner.register(Box::new(
        WasmModule::from_bytes("hello", HELLO_V1).expect("genesis v1 code"),
    ));
    submit(
        &mut joiner,
        0,
        Origin::System,
        modules_msg(&ModulesMsg::RegisterModule {
            module_id: "hello".into(),
            kind: modules::Kind::Module,
            code_hash: sha(HELLO_V1),
        }),
    );
    submit(
        &mut joiner,
        3,
        Origin::System,
        schedule_msg(H, sha(HELLO_REPLACEMENT)),
    );
    submit(
        &mut joiner,
        4,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );
    // any block at H lets the drain inject the boundary Advance that flips
    // the committed active hash — the idempotent re-signal is a harmless
    // carrier op.
    submit(
        &mut joiner,
        H,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );
    assert_eq!(
        joiner.module_root(MODULES_ID).expect("modreg root"),
        modreg_root,
        "the replayed registry converges on the source's committed root"
    );

    // the reconciliation the joiner runs before applying its first block: no
    // pending swap exists, yet the running code must land on the committed ACTIVE.
    let src = MapSource::with(&[HELLO_V1, HELLO_REPLACEMENT]);
    realize(&mut joiner, H + 2, &src).expect("joiner reconciles");
    submit(&mut joiner, H + 2, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(
        count(&joiner),
        100,
        "the joiner runs replacement code (+100), not stale active code"
    );
}

/// crash-restart replay: the registry is disk-durable and reopens AHEAD of the
/// blocks being replayed (active = replacement, no pending left), so the
/// tip's active hash says nothing about which code sealed a pre-swap block.
/// realization keys on the activation HISTORY — a block below `H` re-executes
/// on v1, one at/after `H` on the replacement — never on the tip.
#[test]
fn replay_behind_an_ahead_registry_realizes_the_code_that_sealed_each_block() {
    let (mut host, _) = run_swap_scenario();
    let src = MapSource::with(&[HELLO_V1, HELLO_REPLACEMENT]);
    assert_eq!(
        host.module_code_hash("hello"),
        Some(sha(HELLO_REPLACEMENT)),
        "the live node runs the replacement at the tip"
    );

    // a pre-swap block: back to v1, over the kept state (203 + 1).
    realize(&mut host, H - 1, &src).expect("pre-swap block realizes v1");
    assert_eq!(host.module_code_hash("hello"), Some(sha(HELLO_V1)));
    submit(&mut host, H - 1, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(count(&host), 204, "v1 steps by 1 on the replayed block");

    // the swap block itself and everything after: the replacement.
    realize(&mut host, H, &src).expect("the swap block realizes the replacement");
    assert_eq!(host.module_code_hash("hello"), Some(sha(HELLO_REPLACEMENT)));
    realize(&mut host, H + 5, &src).expect("post-swap is a no-op");
    assert_eq!(host.module_code_hash("hello"), Some(sha(HELLO_REPLACEMENT)));
}

/// the receipt gate at the host seam: a scheduled swap whose readiness never
/// covered the member set does NOT arm past its height — realization is a
/// clean no-op (never a fail-closed error, never a swap) and the drain
/// injects no Advance, however far the height runs.
#[test]
fn unready_swap_never_arms_past_its_height() {
    let mut host = host_with_wasm();
    submit(
        &mut host,
        3,
        Origin::System,
        schedule_msg(H, sha(HELLO_REPLACEMENT)),
    );
    // no SignalReady: the bytes never provably reached the member set.
    let src = MapSource::with(&[HELLO_V1, HELLO_REPLACEMENT]);
    realize(&mut host, H + 100, &src).expect("unready is a no-op, not an error");
    submit(&mut host, H + 100, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(
        count(&host),
        1,
        "active code remains: receipt-gated swaps wait for R=n"
    );
    let (active, pending) = active_hash(&host);
    assert_eq!(active, sha(HELLO_V1), "committed active hash untouched");
    assert!(pending, "the pending swap keeps waiting for its receipts");
}

/// INERT without the registry: a host with only the wasm module realizes nothing
/// and injects nothing — byte-identical drain on a net without modreg.
#[test]
fn inert_without_modreg() {
    let mut host = Host::new();
    host.register(Box::new(
        WasmModule::from_bytes("hello", HELLO_V1).expect("load active component"),
    ));
    let src = MapSource::with(&[HELLO_V1, HELLO_REPLACEMENT]);
    realize(&mut host, H, &src).expect("no registry: nothing to reconcile");
    submit(&mut host, H, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(
        count(&host),
        1,
        "plain active behavior, no swap side-effects"
    );
}

// ---- the RESIDENT's code source: local store first, then the mesh -------------

/// the node-side source shape a resident actually runs: a local content store
/// that does NOT hold the committed bytes, plus a remote that does. `fetch`
/// serves a local hit, else pulls the remote, verifies the digest, and
/// publishes into the local store — `blob_fetch::FetchingCodeSource` in
/// miniature. a resident is not a module-code PUSH fan-out target, so this is
/// the ONLY way it ever holds a committed component.
struct MeshSource {
    local: std::sync::Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
    remote: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl MeshSource {
    /// a resident that holds NOTHING locally and a mesh that serves
    /// `served` — the live shape at a code-swap boundary.
    fn serving(served: &[&[u8]]) -> Self {
        Self {
            local: std::sync::Mutex::new(BTreeMap::new()),
            remote: served.iter().map(|c| (sha(c), deployment(c))).collect(),
        }
    }

    /// a resident whose local store is empty and whose mesh serves nothing:
    /// no peer can answer, so the boundary must fail closed.
    fn unservable() -> Self {
        Self {
            local: std::sync::Mutex::new(BTreeMap::new()),
            remote: BTreeMap::new(),
        }
    }

    fn holds_locally(&self, code_hash: &[u8]) -> bool {
        self.local
            .lock()
            .expect("local store")
            .contains_key(code_hash)
    }
}

#[async_trait::async_trait(?Send)]
impl CodeSource for MeshSource {
    async fn fetch(&self, code_hash: &[u8]) -> Option<Vec<u8>> {
        if let Some(hit) = self.local.lock().expect("local store").get(code_hash) {
            return Some(hit.clone());
        }
        let pulled = self.remote.get(code_hash)?;
        let verified = sha2::Sha256::digest(pulled).as_slice() == code_hash;
        if !verified {
            return None; // content-addressed: an unverified pull is never published.
        }
        self.local
            .lock()
            .expect("local store")
            .insert(code_hash.to_vec(), pulled.clone());
        Some(pulled.clone())
    }

    fn origin(&self) -> &'static str {
        "blob_mesh"
    }
}

/// THE RESIDENT FOLD: a node that never received the PUSH fan-out (its local
/// store is empty at the boundary) realizes the committed swap by pulling the
/// bytes through its source — and publishes them locally on the way, so the
/// next boundary is a local hit. the local-only source over the SAME empty
/// store is the halt this replaces.
#[test]
fn resident_fold_realizes_by_pulling_absent_bytes() {
    let mut host = host_with_wasm();
    submit(
        &mut host,
        3,
        Origin::System,
        schedule_msg(H, sha(HELLO_REPLACEMENT)),
    );
    submit(
        &mut host,
        4,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );

    // the local-only source over an empty store: exactly the halt a resident hit.
    let local_only = MapSource(BTreeMap::new());
    realize(&mut host, H, &local_only).expect_err("a local-only miss fails closed");

    let mesh = MeshSource::serving(&[HELLO_V1, HELLO_REPLACEMENT]);
    assert!(
        !mesh.holds_locally(&sha(HELLO_REPLACEMENT)),
        "the resident holds no bytes for the committed hash before the fold"
    );
    realize(&mut host, H, &mesh).expect("the pulled bytes realize the swap");
    assert!(
        mesh.holds_locally(&sha(HELLO_REPLACEMENT)),
        "a verified pull publishes into the local store"
    );

    submit(&mut host, H, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(count(&host), 100, "the pulled replacement code is what ran");
    let (active, pending) = active_hash(&host);
    assert_eq!(active, sha(HELLO_REPLACEMENT));
    assert!(!pending, "the pending slot is freed at H");
}

/// a hash NO peer can serve is still a hard stop — but the operator gets one
/// line naming the module, the hash and the source that could not serve it
/// BEFORE the fatal, so "unfetchable" reads apart from "never asked".
#[test]
fn unservable_hash_fails_closed_with_the_reason_logged() {
    let mut host = host_with_wasm();
    submit(
        &mut host,
        3,
        Origin::System,
        schedule_msg(H, sha(HELLO_REPLACEMENT)),
    );
    submit(
        &mut host,
        4,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );

    let captured = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let err = tracing::subscriber::with_default(subscriber, || {
        realize(&mut host, H, &MeshSource::unservable()).expect_err("no peer serves it")
    });
    assert!(matches!(err, Error::Module(m) if m.contains("absent")));

    let line = captured.text();
    assert!(
        line.contains("event=\"module_code_unresolved\""),
        "the stable event name must precede the fatal: {line}"
    );
    assert!(
        line.contains("reason=\"code_bytes_absent\""),
        "the stable reason must precede the fatal: {line}"
    );
    assert!(
        line.contains(&hex_lower(&sha(HELLO_REPLACEMENT))),
        "the unfetchable hash must be in the line: {line}"
    );
    assert!(
        line.contains("source=\"blob_mesh\""),
        "the source that could not serve must be in the line: {line}"
    );

    // still fail-closed: nothing swapped.
    submit(&mut host, H, Origin::External(vec![7; 32]), inc_msg());
    assert_eq!(count(&host), 1, "still v1 after the refused boundary");
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// a `MakeWriter` over a shared buffer: the test reads what an operator reads.
#[derive(Clone, Default)]
struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl CapturedLog {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("captured log")).into_owned()
    }
}

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("captured log").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = CapturedLog;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// ALL-OR-NOTHING: two modules swap at the SAME height and this node has only
/// the first one's bytes. The boundary must refuse having changed NOTHING —
/// realizing the first would leave the node running `H`'s code over state still
/// at `H - 1` (the drain turns the Err into a stall that never applies `H`) and
/// answering queries from it.
#[test]
fn a_missing_second_module_realizes_neither() {
    let mut host = host_with_wasm();
    // a SECOND hot-swappable module, sorted AFTER `hello` in the roster — so the
    // resolvable entry is reached first and, unguarded, would swap.
    host.register(Box::new(
        WasmModule::from_bytes("zz-hello", HELLO_V1).expect("load v1"),
    ));
    submit(
        &mut host,
        0,
        Origin::System,
        modules_msg(&ModulesMsg::RegisterModule {
            module_id: "zz-hello".into(),
            kind: modules::Kind::Module,
            code_hash: sha(HELLO_V1),
        }),
    );
    // hello -> the replacement (bytes present); zz-hello -> a hash whose bytes
    // this node does not have. both arm at H.
    let absent = sha(b"a component this node never received");
    for (name, module_id, code_hash) in [
        ("hello-replacement", "hello", sha(HELLO_REPLACEMENT)),
        ("zz-replacement", "zz-hello", absent.clone()),
    ] {
        submit(
            &mut host,
            3,
            Origin::System,
            modules_msg(&ModulesMsg::ScheduleSwap {
                name: name.into(),
                module_id: module_id.into(),
                activation_height: H,
                code_hash: code_hash.clone(),
            }),
        );
        submit(
            &mut host,
            4,
            Origin::External(MEMBER.to_vec()),
            modules_msg(&ModulesMsg::SwapReady {
                name: name.into(),
                module_id: module_id.into(),
                code_hash,
            }),
        );
    }

    // the order the boundary walks: `hello` (resolvable) BEFORE `zz-hello`
    // (absent) is what makes this a partial-realization test at all.
    let req = modules::encode_query(&ModulesQuery::ModuleStatus);
    let bytes = block_on(host.query(MODULES_ID, &req)).expect("status");
    let ModulesReply::ModuleStatus { modules } = modules::decode_reply(&bytes).expect("decode")
    else {
        panic!("expected ModuleStatus");
    };
    let ids: Vec<&str> = modules.iter().map(|m| m.module_id.as_str()).collect();
    assert_eq!(ids, vec!["hello", "zz-hello"], "roster order");

    let root0 = host.root_hash();
    let src = MapSource::with(&[HELLO_V1, HELLO_REPLACEMENT]);
    let err = realize(&mut host, H, &src).expect_err("the absent second entry fails closed");
    assert!(matches!(err, Error::Module(m) if m.contains("absent")));

    assert_eq!(
        host.module_code_hash("hello"),
        Some(sha(HELLO_V1)),
        "the resolvable FIRST module must not have swapped — the boundary is all-or-nothing"
    );
    assert_eq!(host.module_code_hash("zz-hello"), Some(sha(HELLO_V1)));
    assert_eq!(root0, host.root_hash(), "nothing moved the root-hash");

    // Possession and a matching hash are insufficient: compiling the second
    // deployment must also succeed before the first replacement takes effect.
    let mut malformed = MapSource::with(&[HELLO_V1, HELLO_REPLACEMENT]);
    malformed
        .0
        .insert(absent, deployment(b"a component this node never received"));
    realize(&mut host, H, &malformed).expect_err("invalid second component fails closed");
    assert_eq!(host.module_code_hash("hello"), Some(sha(HELLO_V1)));
    assert_eq!(host.module_code_hash("zz-hello"), Some(sha(HELLO_V1)));
    assert_eq!(
        root0,
        host.root_hash(),
        "a compile refusal applies neither swap"
    );
}
