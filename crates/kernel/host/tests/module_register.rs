//! The post-genesis ADMISSION proof: a brand-new wasm module (`kanban`,
//! reusing the `hello` fixture bytes) joins a RUNNING network through the code
//! registry — no node release, no genesis change.
//!
//! What must hold at the admission boundary `H`:
//!   * before `H` the module does not exist anywhere: no registry entry beyond
//!     modreg's admission-pending record (empty active hash), queries fail;
//!   * `realize_module_swaps(H, src)` fetches the committed initial hash's
//!     bytes, verifies sha256, INSTANTIATES the module through the wired
//!     [`ModuleFactory`], and registers it — the root-hash grows by exactly the
//!     new module's (empty) root, identically on every node;
//!   * block `H`'s drain-injected `Advance` flips the committed active hash;
//!   * the module executes from `H` over fresh state;
//!   * a node lacking the bytes, or a host with no factory wired, FAILS
//!     CLOSED — an admission never silently half-lands.

use std::collections::BTreeMap;

use futures::executor::block_on;
use sha2::Digest;

use host::{Admitted, BlockContext, CodeSource, Host, MODULES_ID, ModuleFactory};
use modules::{Modules, ModulesMsg, ModulesQuery, ModulesReply};
use sdk::{Error, Msg, Origin, StateRoot};

const COMPONENT: &[u8] = include_bytes!("fixtures/hello.component.wasm");

/// the admission boundary: far enough past scheduling to clear MIN_SWAP_LEAD.
const H: u64 = 10;

fn deployment(bytes: &[u8]) -> Vec<u8> {
    module_artifact::Artifact::module(bytes.to_vec()).encode()
}

fn sha(bytes: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(deployment(bytes)).to_vec()
}

struct MapSource(BTreeMap<Vec<u8>, Vec<u8>>);

impl MapSource {
    fn with(components: &[&[u8]]) -> Self {
        Self(components.iter().map(|c| (sha(c), deployment(c))).collect())
    }

    /// a blob served exactly as committed — no artifact frame around it.
    fn and_raw(mut self, blob: &[u8]) -> Self {
        self.0.insert(raw_sha(blob), blob.to_vec());
        self
    }
}

fn raw_sha(bytes: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(bytes).to_vec()
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

/// the node-shaped factory: admissions instantiate through the wasm runtime.
struct WasmFactory;

#[async_trait::async_trait(?Send)]
impl ModuleFactory for WasmFactory {
    async fn instantiate(&self, id: &str, bytes: &[u8]) -> Result<Admitted, Error> {
        // the node's own answer, in miniature: a module that will not load
        // HERE stays fail-closed; bytes that are no module at all are another
        // plane's record (see `noded::compose::Admissions`) — including bytes
        // that carry no artifact frame in the first place.
        let Ok(module_artifact::ArtifactRef::Module(artifact)) =
            module_artifact::ArtifactRef::decode(bytes)
        else {
            return Ok(Admitted::ForeignAbi);
        };
        match wasm_host::CompiledModule::compile_artifact(bytes)
            .and_then(|compiled| compiled.over_map(id))
        {
            Ok(module) => Ok(Admitted::Module(Box::new(module))),
            Err(refusal) => match wasm_host::speaks_module_abi(artifact.component) {
                true => Err(refusal),
                false => Ok(Admitted::ForeignAbi),
            },
        }
    }
}

const MEMBER: [u8; 32] = [7; 32];

/// a host with the code registry and a one-member valset — and NO `kanban`
/// anywhere: the module this proof admits does not exist at genesis.
fn bare_host(with_factory: bool) -> Host {
    host_over(Box::new(sdk_testkit::MemStore::new()), with_factory)
}

fn host_over(registry_store: Box<dyn sdk::MerkleStore>, with_factory: bool) -> Host {
    let mut host = Host::new();
    host.register(Box::new(Modules::new(
        MODULES_ID,
        registry_store,
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
    if with_factory {
        host.set_module_factory(Box::new(WasmFactory));
    }
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

fn schedule_register_msg() -> Msg {
    modules_msg(&ModulesMsg::ScheduleRegister {
        name: "kanban-v1".into(),
        module_id: "kanban".into(),
        kind: modules::Kind::Module,
        activation_height: H,
        code_hash: sha(COMPONENT),
        lanes: Vec::new(),
    })
}

fn signal_ready_msg() -> Msg {
    modules_msg(&ModulesMsg::SwapReady {
        name: "kanban-v1".into(),
        module_id: "kanban".into(),
        code_hash: sha(COMPONENT),
    })
}

fn inc_msg() -> Msg {
    Msg {
        target: "kanban".into(),
        payload: b"inc".to_vec(),
    }
}

fn count(host: &Host) -> u64 {
    let bytes = block_on(host.query("kanban", b"")).expect("count query");
    u64::from_le_bytes(bytes.try_into().expect("8-byte count"))
}

fn kanban_entry(host: &Host) -> Option<(Vec<u8>, bool)> {
    let req = modules::encode_query(&ModulesQuery::ModuleStatus);
    let bytes = block_on(host.query(MODULES_ID, &req)).expect("status");
    match modules::decode_reply(&bytes).expect("decode") {
        ModulesReply::ModuleStatus { modules } => modules
            .iter()
            .find(|m| m.module_id == "kanban")
            .map(|m| (m.active_code_hash.clone(), m.pending.is_some())),
        other => panic!("expected Status, got {other:?}"),
    }
}

fn realize(host: &mut Host, height: u64, src: &dyn CodeSource) -> Result<(), Error> {
    block_on(host.realize_module_swaps(height, src))
}

/// drive the WHOLE admission and return the final root-hash — shared by the
/// headline proof and the cross-node determinism check.
fn run_admission_scenario() -> (Host, StateRoot) {
    let mut host = bare_host(true);
    let src = MapSource::with(&[COMPONENT]);

    // governance-shaped admission + the member's byte-receipt signal.
    submit(&mut host, 3, Origin::System, schedule_register_msg());
    submit(
        &mut host,
        4,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );

    // registered-not-running: modreg carries the admission (empty active hash,
    // one pending), the host does not know the module at all.
    let (active, pending) = kanban_entry(&host).expect("admission landed");
    assert!(active.is_empty(), "no active code before the boundary");
    assert!(pending, "the pending initial code is the admission");
    assert!(
        block_on(host.query("kanban", b"")).is_err(),
        "the module does not answer before its boundary"
    );

    // below H nothing arms: realization is a no-op and the module stays absent.
    realize(&mut host, H - 1, &src).expect("below H is Ok");
    assert!(
        host.module_root("kanban").is_none(),
        "not registered below H"
    );

    // THE BOUNDARY: realization instantiates + registers, growing the root-hash
    // by the new module's (empty) root — deterministically.
    let root_hash_before = host.root_hash();
    realize(&mut host, H, &src).expect("admission realizes at H");
    assert!(host.module_root("kanban").is_some(), "registered at H");
    assert_ne!(
        host.root_hash(),
        root_hash_before,
        "unlike a swap, an admission changes the registry set and thus the root-hash"
    );
    // idempotent: a second realization at the same height is a no-op.
    let after_first = host.root_hash();
    realize(&mut host, H, &src).expect("re-realize is Ok");
    assert_eq!(
        host.root_hash(),
        after_first,
        "re-realization moves nothing"
    );

    // block H: the module executes over fresh state; the drain's injected
    // Advance flips the committed active hash in the same block.
    submit(&mut host, H, Origin::External(vec![9; 32]), inc_msg());
    assert_eq!(count(&host), 1, "fresh state, first inc");
    let (active, pending) = kanban_entry(&host).expect("entry persists");
    assert_eq!(
        active,
        sha(COMPONENT),
        "Advance flipped the committed hash at H"
    );
    assert!(!pending, "the pending slot is freed at H");

    // after the boundary the module is an ordinary hot-swappable citizen.
    realize(&mut host, H + 1, &src).expect("post-H realize is Ok");
    submit(&mut host, H + 1, Origin::External(vec![9; 32]), inc_msg());
    assert_eq!(count(&host), 2);

    let final_hash = host.root_hash();
    (host, final_hash)
}

/// the smallest compliant module — two exports, no state, no events, no
/// emitted messages — admits like any other and costs the network exactly one
/// registry entry and one empty root in the root-hash: it runs over the empty
/// store, an op is accepted as a no-op that never moves that root, and a
/// query answers empty.
#[test]
fn a_module_that_touches_nothing_admits_over_the_empty_root_and_never_moves_it() {
    const NOOP: &[u8] = include_bytes!("fixtures/noop.component.wasm");
    let mut host = bare_host(true);
    let src = MapSource::with(&[NOOP]);
    submit(
        &mut host,
        3,
        Origin::System,
        modules_msg(&ModulesMsg::ScheduleRegister {
            name: "noop-v1".into(),
            module_id: "noop".into(),
            kind: modules::Kind::Module,
            activation_height: H,
            code_hash: sha(NOOP),
            lanes: Vec::new(),
        }),
    );
    submit(
        &mut host,
        4,
        Origin::External(MEMBER.to_vec()),
        modules_msg(&ModulesMsg::SwapReady {
            name: "noop-v1".into(),
            module_id: "noop".into(),
            code_hash: sha(NOOP),
        }),
    );
    realize(&mut host, H, &src).expect("admission realizes at H");
    let (_, empty_root) = wasm_host::initial_state(&[]);
    assert_eq!(
        host.module_root("noop"),
        Some(empty_root),
        "admitted over the empty store"
    );

    let any_op = Msg {
        target: "noop".into(),
        payload: b"anything".to_vec(),
    };
    submit(&mut host, H, Origin::External(vec![9; 32]), any_op);
    assert_eq!(
        host.module_root("noop"),
        Some(empty_root),
        "an accepted op moves nothing"
    );
    let reply = block_on(host.query("noop", b"anything")).expect("a query answers");
    assert!(reply.is_empty(), "the answer is empty, got {reply:?}");
}

/// the headline proof: a module that did not exist at genesis goes LIVE at `H`
/// through governance-shaped ops alone.
#[test]
fn admission_at_boundary_instantiates_and_runs_the_new_module() {
    run_admission_scenario();
}

/// two independent nodes running the identical finalized sequence land on the
/// identical root-hash — admission introduces no per-node divergence.
#[test]
fn admission_is_deterministic_across_nodes() {
    let (_, a) = run_admission_scenario();
    let (_, b) = run_admission_scenario();
    assert_eq!(a, b, "identical histories, identical root-hashes");
}

/// a node that does not hold the bytes at the boundary FAILS CLOSED.
#[test]
fn admission_fails_closed_on_missing_or_tampered_bytes() {
    let mut host = bare_host(true);
    submit(&mut host, 3, Origin::System, schedule_register_msg());
    submit(
        &mut host,
        4,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );

    let empty = MapSource::with(&[]);
    assert!(
        realize(&mut host, H, &empty).is_err(),
        "absent bytes must stop the boundary"
    );
    assert!(host.module_root("kanban").is_none(), "nothing half-landed");

    // tampered bytes: right key in the map, wrong content.
    let mut tampered = MapSource::with(&[]);
    tampered.0.insert(sha(COMPONENT), b"evil".to_vec());
    assert!(
        realize(&mut host, H, &tampered).is_err(),
        "hash-mismatched bytes must stop the boundary"
    );
    assert!(host.module_root("kanban").is_none(), "nothing half-landed");

    // and the same host still admits fine once the bytes appear.
    let src = MapSource::with(&[COMPONENT]);
    realize(&mut host, H, &src).expect("healed fetch admits");
    assert!(host.module_root("kanban").is_some());
}

/// a host with no factory wired FAILS CLOSED the moment an admission arms —
/// never before.
#[test]
fn admission_fails_closed_without_a_module_factory() {
    let mut host = bare_host(false);
    let src = MapSource::with(&[COMPONENT]);
    // inert while nothing is admitted.
    realize(&mut host, H, &src).expect("no admissions, no factory needed");

    submit(&mut host, 3, Origin::System, schedule_register_msg());
    submit(
        &mut host,
        4,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );
    realize(&mut host, H - 1, &src).expect("unarmed admission needs nothing");
    assert!(
        realize(&mut host, H, &src).is_err(),
        "an armed admission with no factory must stop the boundary"
    );
}

/// THE REGISTRY IS ID-GENERIC AND THE BOUNDARY IS NOT. A record whose code is
/// not a `ducktape:module` — the reachability plane's `ducktape:netstack`
/// guest, delivered through the very same governance path — is SKIPPED here:
/// it is not this boundary's to instantiate, and treating it as an admission
/// would fail closed on every node, on every block, forever. The entry stays
/// committed (its own plane's non-blocking reconciler reads it), the root-hash
/// does not move, and a real admission in the same set still lands.
#[test]
fn a_foreign_abi_record_is_skipped_and_the_boundary_keeps_sealing() {
    const NETSTACK: &[u8] = include_bytes!("../../../networking/netstack-machine/component.wasm");

    let mut host = bare_host(true);
    let src = MapSource::with(&[COMPONENT, NETSTACK]);
    for (name, id, code) in [
        ("netstack-v1", "netstack", NETSTACK),
        ("kanban-v1", "kanban", COMPONENT),
    ] {
        submit(
            &mut host,
            3,
            Origin::System,
            modules_msg(&ModulesMsg::ScheduleRegister {
                name: name.into(),
                module_id: id.into(),
                kind: modules::Kind::Module,
                activation_height: H,
                code_hash: sha(code),
                lanes: Vec::new(),
            }),
        );
        submit(
            &mut host,
            4,
            Origin::External(MEMBER.to_vec()),
            modules_msg(&ModulesMsg::SwapReady {
                name: name.into(),
                module_id: id.into(),
                code_hash: sha(code),
            }),
        );
    }

    let root_hash_before = host.root_hash();
    realize(&mut host, H, &src).expect("a foreign-abi record must not stop the boundary");
    assert!(
        host.module_root("netstack").is_none(),
        "nothing seated for another plane's component"
    );
    assert!(
        host.module_root("kanban").is_some(),
        "the real admission still lands"
    );
    assert_ne!(
        host.root_hash(),
        root_hash_before,
        "by exactly the real admission"
    );

    // the block seals: its ops apply and the drain-injected Advance flips both
    // committed hashes — the netstack record stays in the registry, which is
    // the whole point of committing it there.
    submit(&mut host, H, Origin::External(vec![9; 32]), inc_msg());
    assert_eq!(count(&host), 1, "the block applied");
    let req = modules::encode_query(&ModulesQuery::ModuleStatus);
    let bytes = block_on(host.query(MODULES_ID, &req)).expect("status");
    let ModulesReply::ModuleStatus { modules } = modules::decode_reply(&bytes).expect("decode")
    else {
        panic!("expected ModuleStatus")
    };
    let netstack = modules
        .iter()
        .find(|m| m.module_id == "netstack")
        .expect("the netstack record persists");
    assert_eq!(
        netstack.active_code_hash,
        sha(NETSTACK),
        "the registry carries the designated netstack code for its own plane to read"
    );

    // and every later boundary is a latched no-op — never a re-decision, never
    // an error.
    let sealed = host.root_hash();
    for height in [H, H + 1, H + 2] {
        realize(&mut host, height, &src).expect("later boundaries stay Ok");
    }
    assert_eq!(host.root_hash(), sealed, "the skip moves nothing");
    submit(&mut host, H + 2, Origin::External(vec![9; 32]), inc_msg());
    assert_eq!(count(&host), 2, "and blocks keep sealing");
}

/// THE SAME ANSWER ONE STEP EARLIER. The registry commits a hash, not a
/// shape: a blob that is not a `ModuleArtifact` frame at all — a bare
/// component, or any other plane's bytes — is another plane's record too. The
/// failure is a pure function of committed bytes, so a hard error here is not
/// a refused swap but a chain that never applies the activation block again,
/// on every honest node, with no governance op able to reach the fix.
#[test]
fn a_non_frame_registry_blob_is_skipped_and_the_boundary_keeps_sealing() {
    // a bare wasm preamble: valid-looking bytes, no artifact frame.
    const RAW: &[u8] = b"\0asm\x01\0\0\0";

    let mut host = bare_host(true);
    let src = MapSource::with(&[COMPONENT]).and_raw(RAW);
    for (name, id, code_hash) in [
        ("blob-v1", "blob", raw_sha(RAW)),
        ("kanban-v1", "kanban", sha(COMPONENT)),
    ] {
        submit(
            &mut host,
            3,
            Origin::System,
            modules_msg(&ModulesMsg::ScheduleRegister {
                name: name.into(),
                module_id: id.into(),
                kind: modules::Kind::Module,
                activation_height: H,
                code_hash: code_hash.clone(),
                lanes: Vec::new(),
            }),
        );
        submit(
            &mut host,
            4,
            Origin::External(MEMBER.to_vec()),
            modules_msg(&ModulesMsg::SwapReady {
                name: name.into(),
                module_id: id.into(),
                code_hash,
            }),
        );
    }

    realize(&mut host, H, &src).expect("an unframed blob must not stop the boundary");
    assert!(
        host.module_root("blob").is_none(),
        "nothing seated for bytes that are no module"
    );
    assert!(
        host.module_root("kanban").is_some(),
        "the real admission still lands"
    );
    submit(&mut host, H, Origin::External(vec![9; 32]), inc_msg());
    assert_eq!(count(&host), 1, "the block applied");
}

/// a registry store whose reads fail while armed — the node-local read
/// failure the wasm registry's query can hit for reasons that are nobody's
/// consensus decision (fuel, a store read, instantiation).
struct FlakyStore {
    inner: sdk_testkit::MemStore,
    failing: std::rc::Rc<std::cell::Cell<bool>>,
}

#[async_trait::async_trait(?Send)]
impl sdk::MerkleStore for FlakyStore {
    async fn get(&self, key: &[u8; sdk::ROOT_LEN]) -> Result<Option<Vec<u8>>, Error> {
        if self.failing.get() {
            return Err(Error::module(
                "injected_fault",
                "injected registry read failure",
            ));
        }
        self.inner.get(key).await
    }

    async fn commit_batch(
        &mut self,
        writes: Vec<([u8; sdk::ROOT_LEN], Option<Vec<u8>>)>,
    ) -> Result<(), Error> {
        self.inner.commit_batch(writes).await
    }

    fn root(&self) -> StateRoot {
        self.inner.root()
    }

    async fn sync_target(&self) -> Result<sdk::ResolverSyncTarget, Error> {
        self.inner.sync_target().await
    }

    async fn serve_sync(&self, req: &[u8]) -> Result<Vec<u8>, Error> {
        self.inner.serve_sync(req).await
    }
}

/// A FAILED REGISTRY READ IS A STALL, NOT AN EMPTY REGISTRY. The registry is
/// a wasm deployment now, so its query fails node-locally for reasons that are
/// nobody's committed decision. Reading such a failure as "no registry" would
/// realize no swap and inject no `Advance` — this node then seals a block on
/// stale code and a different root than its peers, and only learns it later as
/// a hard root-hash mismatch. It must fail closed and retry instead.
#[test]
fn a_failed_registry_query_stalls_the_boundary() {
    let failing = std::rc::Rc::new(std::cell::Cell::new(false));
    let mut host = host_over(
        Box::new(FlakyStore {
            inner: sdk_testkit::MemStore::new(),
            failing: failing.clone(),
        }),
        true,
    );
    let src = MapSource::with(&[COMPONENT]);
    submit(&mut host, 3, Origin::System, schedule_register_msg());
    submit(
        &mut host,
        4,
        Origin::External(MEMBER.to_vec()),
        signal_ready_msg(),
    );

    failing.set(true);
    let err = realize(&mut host, H, &src).expect_err("a failed registry read must not look empty");
    assert!(
        err.to_string().contains("injected registry read failure"),
        "the read failure propagates verbatim: {err}"
    );
    assert!(
        host.module_root("kanban").is_none(),
        "nothing realized under a failed read"
    );

    // and the in-block half fails closed too: no block seals without the
    // `Advance` tick the injection could not compute.
    let ctx = BlockContext {
        height: H,
        consensus_time: H,
        origin: Origin::External(vec![9; 32]),
    };
    let err = block_on(host.submit_at(ctx, inc_msg()))
        .expect_err("the drain must stall on an unreadable registry");
    assert!(
        err.to_string().contains("injected registry read failure"),
        "the drain stalls on THAT failure, not on the absent module: {err}"
    );

    // the failure was node-local and transient: the retry lands.
    failing.set(false);
    realize(&mut host, H, &src).expect("the retry realizes the admission");
    submit(&mut host, H, Origin::External(vec![9; 32]), inc_msg());
    assert_eq!(count(&host), 1, "and the block applies");
}

/// an admission that never latches ready never arms — however high the height.
#[test]
fn unready_admission_never_arms() {
    let mut host = bare_host(true);
    let src = MapSource::with(&[COMPONENT]);
    submit(&mut host, 3, Origin::System, schedule_register_msg());
    // no SignalReady.
    realize(&mut host, H + 100, &src).expect("unready is a no-op");
    assert!(host.module_root("kanban").is_none());
    let (active, pending) = kanban_entry(&host).expect("entry persists");
    assert!(active.is_empty());
    assert!(pending, "still waiting on readiness");
}
