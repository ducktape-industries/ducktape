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
//!     CLOSED — an admission never silently half-lands;
//!   * WHO seats an entry is the record's COMMITTED kind, never the bytes: a
//!     [`modules::Kind::Plane`] record is passed over on every node, and a
//!     [`modules::Kind::Module`] this build cannot load stalls the boundary on
//!     every node — two builds over one history can never seat two registries.

use std::collections::BTreeMap;

use futures::executor::block_on;
use sha2::Digest;

use host::{Admitted, BlockContext, CodeSource, Host, MODULES_ID, ModuleFactory};
use modules::{Modules, ModulesMsg, ModulesQuery, ModulesReply};
use sdk::{Error, Module, Msg, Origin, StateRoot};

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
        // the node's own answer, in miniature: the entry's COMMITTED kind
        // already said these are a module's bytes, so every way they fail to
        // become one — no artifact frame, a frame this runtime cannot compile,
        // a component that exports something else — is an Err and the boundary
        // stalls. Answering "not a module" here is what forks a network.
        let module = wasm_host::CompiledModule::compile_artifact(bytes)
            .and_then(|compiled| compiled.over_map(id))?;
        Ok(Admitted::Module(Box::new(module)))
    }

    // an admission over a map touches nothing but itself: it is its own scratch.
    fn check(&self, id: &str, bytes: &[u8]) -> Result<(), Error> {
        futures::executor::block_on(self.instantiate(id, bytes)).map(drop)
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

/// registers `id` at [`H`] under `kind`, and signals it ready.
fn arm(host: &mut Host, name: &str, id: &str, kind: modules::Kind, code_hash: Vec<u8>) {
    submit(
        host,
        3,
        Origin::System,
        modules_msg(&ModulesMsg::ScheduleRegister {
            name: name.into(),
            module_id: id.into(),
            kind,
            activation_height: H,
            code_hash: code_hash.clone(),
            lanes: Vec::new(),
        }),
    );
    submit(
        host,
        4,
        Origin::External(MEMBER.to_vec()),
        modules_msg(&ModulesMsg::SwapReady {
            name: name.into(),
            module_id: id.into(),
            code_hash,
        }),
    );
}

/// THE REGISTRY IS ID-GENERIC AND THE BOUNDARY IS NOT — AND THE COMMITTED KIND
/// SAYS WHICH IS WHICH. A [`modules::Kind::Plane`] record — the reachability
/// plane's `ducktape:netstack` guest, delivered through the very same
/// governance path — is passed over here: the committed record already says
/// another plane realizes it, so no node ever asks its factory what the bytes
/// ARE. The entry stays committed (its own plane's non-blocking reconciler
/// reads it) and a real admission in the same set still lands.
#[test]
fn a_plane_record_is_passed_over_and_the_boundary_keeps_sealing() {
    const NETSTACK: &[u8] = include_bytes!("../../../networking/netstack-machine/component.wasm");

    let mut host = bare_host(true);
    let src = MapSource::with(&[COMPONENT, NETSTACK]);
    arm(
        &mut host,
        "netstack-v1",
        "netstack",
        modules::Kind::Plane,
        sha(NETSTACK),
    );
    arm(
        &mut host,
        "kanban-v1",
        "kanban",
        modules::Kind::Module,
        sha(COMPONENT),
    );

    let root_hash_before = host.root_hash();
    realize(&mut host, H, &src).expect("a plane record must not stop the boundary");
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
    assert_eq!(host.root_hash(), sealed, "the pass-over moves nothing");
    submit(&mut host, H + 2, Origin::External(vec![9; 32]), inc_msg());
    assert_eq!(count(&host), 2, "and blocks keep sealing");
}

/// A COMMITTED MODULE THIS BINARY CANNOT SEAT STOPS THE BOUNDARY. The registry
/// commits a hash AND a kind: bytes registered as a [`modules::Kind::Module`]
/// that are not a `ModuleArtifact` frame at all — a bare component, or any
/// other plane's bytes — are a module this build failed to load, never "not a
/// module". The boundary fails closed and retries forever, which is a stall the
/// operator sees; the alternative, skipping what this build could not read,
/// seats a different registry on a node whose build could.
#[test]
fn a_committed_module_this_binary_cannot_seat_stops_the_boundary() {
    // a bare wasm preamble: valid-looking bytes, no artifact frame.
    const RAW: &[u8] = b"\0asm\x01\0\0\0";

    let mut host = bare_host(true);
    let src = MapSource::with(&[COMPONENT]).and_raw(RAW);
    arm(
        &mut host,
        "blob-v1",
        "blob",
        modules::Kind::Module,
        raw_sha(RAW),
    );
    arm(
        &mut host,
        "kanban-v1",
        "kanban",
        modules::Kind::Module,
        sha(COMPONENT),
    );

    let root_hash_before = host.root_hash();
    assert!(
        realize(&mut host, H, &src).is_err(),
        "bytes committed as a module that will not load must stop the boundary"
    );
    assert!(
        host.module_root("blob").is_none(),
        "nothing seated for bytes that would not load"
    );
    assert_eq!(
        host.root_hash(),
        root_hash_before,
        "a stalled boundary commits nothing at all"
    );

    // and it stays stopped: the committed record is what it is, so every retry
    // answers the same way.
    assert!(
        realize(&mut host, H, &src).is_err(),
        "the stall is a pure function of committed state"
    );
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

/// a module that loads fine and refuses once it is started.
struct RefusesToStart;

#[async_trait::async_trait(?Send)]
impl Module for RefusesToStart {
    fn id(&self) -> sdk::ModuleId {
        "kanban".into()
    }

    fn root(&self) -> StateRoot {
        StateRoot([0; 32])
    }

    async fn execute(&mut self, _ctx: &mut dyn sdk::Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }

    async fn initialize(&mut self, _params: &[u8]) -> Result<(), Error> {
        Err(Error::module("not_configured", "no board to start from"))
    }
}

/// the node's factory in miniature (`noded::compose::Admissions`): an
/// admission is seated only once its `initialize` has run, and a refusal there
/// is the admission's refusal.
struct StartingFactory;

#[async_trait::async_trait(?Send)]
impl ModuleFactory for StartingFactory {
    async fn instantiate(&self, id: &str, _bytes: &[u8]) -> Result<Admitted, Error> {
        let mut module = RefusesToStart;
        module
            .initialize(&[])
            .await
            .map_err(|e| Error::module("module_seat", format!("{id} initializes: {e}")))?;
        Ok(Admitted::Module(Box::new(module)))
    }

    fn check(&self, id: &str, bytes: &[u8]) -> Result<(), Error> {
        futures::executor::block_on(self.instantiate(id, bytes)).map(drop)
    }
}

/// AN ADMISSION THAT CANNOT START NEVER ARMS. `initialize` runs when an
/// admission is seated at its boundary, identically on every node — so a guest
/// that refuses there stops every node at that height, retrying forever. The
/// readiness question each validator asks before it signals is that same
/// admission over scratch state: it refuses, nobody signals, the admission
/// never arms, and every block past its height applies on every node.
#[test]
fn an_admission_whose_initialize_fails_never_arms_and_every_block_applies() {
    let run_node = || {
        let mut host = bare_host(false);
        host.register(Box::new(directory::Directory::new("directory")));
        host.set_module_factory(Box::new(StartingFactory));
        let src = MapSource::with(&[COMPONENT]);
        submit(&mut host, 3, Origin::System, schedule_register_msg());

        // the validator's readiness question, as its node asks it: only a
        // ready answer signs `SwapReady`.
        let ready = host.check_module_replacement("kanban", &deployment(COMPONENT));
        if ready.is_ok() {
            submit(
                &mut host,
                4,
                Origin::External(MEMBER.to_vec()),
                signal_ready_msg(),
            );
        }
        for height in H - 1..=H + 2 {
            realize(&mut host, height, &src).expect("every boundary realizes");
            submit(
                &mut host,
                height,
                Origin::External(vec![9; 32]),
                Msg {
                    target: "directory".into(),
                    payload: directory::encode_msg(&directory::DirMsg::Set {
                        key: format!("block-{height}"),
                        value: "applied".into(),
                    }),
                },
            );
        }
        assert!(host.module_root("kanban").is_none(), "nothing seated");
        let (active, pending) = kanban_entry(&host).expect("the admission stays recorded");
        assert!(active.is_empty(), "never activated");
        assert!(
            pending,
            "still pending, never ready — governance can cancel it"
        );
        let refusal = ready.expect_err("an admission that cannot start is not ready");
        assert!(
            refusal.to_string().contains("kanban initializes"),
            "the refusal names the module and the step: {refusal}"
        );
        host.root_hash()
    };
    assert_eq!(run_node(), run_node(), "every node lands on the same root");
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

/// a module that seats from any bytes at all.
struct Anything;

#[async_trait::async_trait(?Send)]
impl Module for Anything {
    fn id(&self) -> sdk::ModuleId {
        "blob".into()
    }

    fn root(&self) -> StateRoot {
        StateRoot([7; 32])
    }

    async fn execute(&mut self, _ctx: &mut dyn sdk::Ctx, _msg: &Msg) -> Result<(), Error> {
        Ok(())
    }

    async fn initialize(&mut self, _params: &[u8]) -> Result<(), Error> {
        Ok(())
    }
}

/// a build whose runtime loads bytes this repo's `wasm_host` will not.
struct SeatsAnything;

#[async_trait::async_trait(?Send)]
impl ModuleFactory for SeatsAnything {
    async fn instantiate(&self, _id: &str, _bytes: &[u8]) -> Result<Admitted, Error> {
        Ok(Admitted::Module(Box::new(Anything)))
    }

    fn check(&self, _id: &str, _bytes: &[u8]) -> Result<(), Error> {
        Ok(())
    }
}

/// TWO BINARIES OVER ONE HISTORY NEVER BOTH ADVANCE WITH DIFFERENT REGISTRIES.
/// whether a build's runtime can load a given artifact is a property of the
/// BUILD, not of committed state: the same bytes one node seats, an older or
/// newer one may not. so the answer "not for me" may never be a quiet skip —
/// the node that cannot seat what the record commits as a module STOPS, and a
/// stall commits nothing. the two nodes' registries stay equal because only
/// one of them moved at all; before #2705 both returned `Ok`, both sealed the
/// block, and their root hashes forked.
#[test]
fn two_builds_that_disagree_about_bytes_never_seat_two_registries() {
    const RAW: &[u8] = b"\0asm\x01\0\0\0";
    let src = MapSource::with(&[COMPONENT]).and_raw(RAW);

    // one committed history, replayed on both builds.
    let arm_both = |host: &mut Host| {
        arm(host, "blob-v1", "blob", modules::Kind::Module, raw_sha(RAW));
    };

    let mut loads = bare_host(false);
    loads.set_module_factory(Box::new(SeatsAnything));
    arm_both(&mut loads);

    let mut refuses = bare_host(true);
    arm_both(&mut refuses);

    let agreed = loads.root_hash();
    assert_eq!(
        refuses.root_hash(),
        agreed,
        "the same committed history, up to the boundary"
    );

    realize(&mut loads, H, &src).expect("the build that loads these bytes seats them");
    realize(&mut refuses, H, &src).expect_err("the build that cannot load them stops the boundary");

    assert!(loads.module_root("blob").is_some(), "seated on the one");
    assert!(
        refuses.module_root("blob").is_none(),
        "nothing seated on the other"
    );
    assert_ne!(
        loads.root_hash(),
        agreed,
        "the seating node moved past the boundary"
    );
    assert_eq!(
        refuses.root_hash(),
        agreed,
        "the refusing node did not move at all — a stall, not a second registry"
    );
}
