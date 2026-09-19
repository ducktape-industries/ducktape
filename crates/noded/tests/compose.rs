//! the composer over REAL substrates: the repo's fixture components, qmdb
//! stores on a temp runtime, an empty blob plane — genesis, then a reopen of
//! the same stores off the modules registry with a map snapshot re-installed;
//! and the ONE wasm path's refusals, by name.

use std::path::PathBuf;
use std::time::Duration;

use commonware_runtime::Runner as _;
use host::CapturePayloads;
use noded::bundle::{DirCodeSource, qmdb_stores};
use noded::compose::{
    Admissions, Bindings, Boot, BoxFut, Start, Substrates, check_realizable, compose, wasm_module,
};
use sdk::{Module, StateRoot, StateSyncHandle};
use sdk_testkit::TestCtx;
use wasm_host::{Backing, Shape};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../kernel/host/tests/fixtures")
}

/// the object-storage tenant fixture, committed here rather than read out of
/// the kernel fixture set: `declared_shapes` pins that directory to the set the
/// host composes, and this guest is a test tenant no network founds.
fn object_component() -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/object.component.wasm"),
    )
    .unwrap()
}

/// one `SnapshotSource` call's future.
type SnapshotFut<'a> = BoxFut<'a, Result<Option<(Vec<u8>, StateRoot)>, String>>;

/// `directory` is the MAP tenant here, and the only one: the reopen half of
/// [`composes_only_wasm_over_injected_stores`] proves a map installs its
/// snapshot while a store is never asked, so the selection needs one of each
/// and every other id below is store-backed.
const SELECTION: &[&str] = &[
    "kv",
    "valset",
    "acl",
    "governance",
    "modules",
    "runs",
    "directory",
];

const BINDINGS: Bindings<'static> = Bindings {
    invite: b"t",
    chain_id: "t",
    time_unit: sdk::genesis_config::TimeUnit::Height,
};

fn run(body: impl FnOnce(commonware_runtime::tokio::Context, PathBuf) -> BoxFut<'static, ()>) {
    let dir = tempfile::tempdir().unwrap();
    let cfg =
        commonware_runtime::tokio::Config::default().with_storage_directory(dir.path().join("s"));
    let root = dir.path().to_path_buf();
    commonware_runtime::tokio::Runner::new(cfg).start(|context| body(context, root));
}

fn substrates(dir: &std::path::Path) -> Substrates {
    Substrates {
        directory: dir.join("module-storage"),
        bindings: [
            ("forge".into(), dir.join("forge")),
            ("files".into(), dir.join("duckfs")),
        ]
        .into(),
        blobs: blobstore::BlobHandle::default(),
    }
}

#[test]
fn composes_only_wasm_over_injected_stores() {
    run(|context, dir| {
        Box::pin(async move {
            let (code, by_id) = DirCodeSource::open(&fixtures(), SELECTION).unwrap();
            let validators = vec![vec![7u8; 32]];
            let substrates = substrates(&dir);
            let mut stores = qmdb_stores(&context);

            // ---- genesis ----
            let genesis = compose(
                &code,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Genesis {
                    validators: &validators,
                    bundle: &by_id,
                },
            )
            .await
            .unwrap();
            let mut ids: Vec<String> = genesis
                .module_roots()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            ids.sort_unstable();
            let mut want: Vec<String> = SELECTION.iter().map(|s| s.to_string()).collect();
            want.sort_unstable();
            assert_eq!(ids, want);
            for id in SELECTION {
                assert_eq!(genesis.module_code_hash(id).unwrap(), by_id[*id]);
            }
            assert_eq!(
                genesis.module_code_hash("acl").unwrap(),
                by_id["acl"].to_vec()
            );
            // acl and governance are both store-backed and empty at genesis
            // EXCEPT for governance's seeded `__config` record — the only
            // thing that can set their roots apart.
            assert_ne!(
                genesis.module_root("governance"),
                genesis.module_root("acl"),
                "governance's genesis config was seeded into its store"
            );
            let genesis_root = genesis.root_hash();
            let (captured, _) =
                genesis.capture_current_snapshot(0, CapturePayloads::All, || Duration::ZERO);
            let map = captured.module("directory").expect("directory composed");
            let StateSyncHandle::SnapshotBytes(map_snapshot) = map.state_sync.clone() else {
                panic!("a map tenant syncs by snapshot bytes");
            };
            let map_root = map.root;
            drop(genesis);

            // ---- reopen the same stores at block zero: the wasm set comes off
            // the modules registry, no store re-seeds, the map tenant installs
            // its snapshot, and the composed root-hash is the genesis one ----
            let mut snapshots = |id: &str, backing: Backing| -> SnapshotFut<'_> {
                assert_ne!(
                    backing,
                    Backing::Store,
                    "a store-backed module is never asked"
                );
                let bytes = map_snapshot.clone();
                let is_map = id == "directory";
                Box::pin(async move { Ok(is_map.then_some((bytes, map_root))) })
            };
            let reopened = compose(
                &code,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Reopen {
                    height: 0,
                    codes: &by_id,
                    snapshots: &mut snapshots,
                },
            )
            .await
            .unwrap();
            assert_eq!(
                reopened.root_hash(),
                genesis_root,
                "reopen composes the genesis root-hash"
            );
        })
    });
}

#[test]
fn genesis_module_membership_comes_from_the_bundle() {
    run(|context, dir| {
        Box::pin(async move {
            let (code, by_id) = DirCodeSource::open(&fixtures(), &["directory"]).unwrap();
            let substrates = substrates(&dir);
            let mut stores = qmdb_stores(&context);
            let genesis = compose(
                &code,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Genesis {
                    validators: &[],
                    bundle: &by_id,
                },
            )
            .await
            .unwrap();
            assert!(genesis.module_root("directory").is_some());
            assert!(genesis.module_root("acl").is_none());
            assert!(genesis.module_root("runs").is_none());
            assert!(
                genesis.module_status().await.unwrap().is_none(),
                "an omitted registry is not inserted by the binary"
            );
            assert_eq!(genesis.module_roots().len(), 1);
        })
    });
}

/// genesis declares no lane of its own. The lane table a founding module
/// lands in is the one its FRAME carries — read off the same bytes the code
/// hash covers — so there is no native table beside the deployment to
/// disagree with it, and a frame that declares nothing founds no lane.
#[test]
fn founding_lanes_come_from_the_frame_and_nowhere_else() {
    use module_artifact::{Artifact, ModuleArtifact};
    run(|context, dir| {
        Box::pin(async move {
            let telemetry = modules::LaneDecl {
                id: 7,
                name: "telemetry".into(),
                stream: Some(modules::LaneStream {
                    pacing: modules::LanePacing::Shared,
                    accept_backlog: 16,
                }),
            };
            let read = |id: &str| std::fs::read(fixtures().join(format!("{id}.component.wasm")));
            let mut source = ArtifactSource(Default::default());
            let codes = std::collections::BTreeMap::from([
                // the registry's own frame declares nothing…
                (
                    "modules".to_string(),
                    source.add(Artifact::module(read("modules").unwrap())),
                ),
                // …and this one declares one lane, in its artifact.
                (
                    "valset".to_string(),
                    source.add(Artifact::Module(ModuleArtifact {
                        component: read("valset").unwrap(),
                        index: None,
                        view: None,
                        lanes: vec![telemetry.clone()],
                    })),
                ),
            ]);

            let substrates = substrates(&dir);
            let mut stores = qmdb_stores(&context);
            let host = compose(
                &source,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Genesis {
                    validators: &[],
                    bundle: &codes,
                },
            )
            .await
            .unwrap();

            let reply = host
                .query(
                    host::MODULES_ID,
                    &modules::encode_query(&modules::ModulesQuery::Lanes),
                )
                .await
                .unwrap();
            let modules::ModulesReply::Lanes { lanes } = modules::decode_reply(&reply).unwrap()
            else {
                panic!("the lane query answers with lanes");
            };
            assert_eq!(
                lanes,
                vec![modules::LaneRecord {
                    id: telemetry.id,
                    module_id: "valset".into(),
                    name: telemetry.name.clone(),
                    stream: telemetry.stream.clone(),
                }],
                "the committed table is the frame's declaration, and only it"
            );
        })
    });
}

#[test]
fn genesis_refuses_unsafe_ids() {
    run(|context, dir| {
        Box::pin(async move {
            let (code, by_id) = DirCodeSource::open(&fixtures(), &["directory"]).unwrap();
            let substrates = substrates(&dir);
            let mut stores = qmdb_stores(&context);
            for id in ["../outside", "bad/id"] {
                let bundle =
                    std::collections::BTreeMap::from([(id.to_string(), by_id["directory"])]);
                let result = compose(
                    &code,
                    &mut stores,
                    &substrates,
                    &BINDINGS,
                    Boot::Genesis {
                        validators: &[],
                        bundle: &bundle,
                    },
                )
                .await;
                assert!(result.is_err(), "{id}");
            }
        })
    });
}

/// a guest built against a `ducktape:module/host` world this build has moved
/// past: the real forge component from before the host widened
/// `git-object-read`'s commit record from two fields to five. The fault is in
/// the fixture's own bytes — nothing here plants one.
fn stale_host_wit() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stale-host-wit")
}

/// [`compose::load`] IS the linker gate, and it is the WHOLE of what a lock-free
/// preflight can ask a binary: no store, no substrate, no byte of state — just
/// whether the components a network runs still link. So a component whose
/// imports this build's world no longer satisfies must be refused HERE, in the
/// sentence a boot would print, or `release schedule`'s preflight has nothing
/// to read and every launcher pays a node stop to learn the same answer.
#[test]
fn a_component_built_against_another_host_world_does_not_load() {
    let stale = workspace_config::read_module_artifact(&stale_host_wit(), "forge")
        .unwrap()
        .encode();
    let Err(refusal) = noded::compose::load("forge", &stale) else {
        panic!("a component built against another host world must not load");
    };
    assert!(
        refusal.starts_with("forge component loads: "),
        "the compose sentence, verbatim: {refusal}"
    );
    assert!(
        refusal.contains("a matching implementation was not found in the linker"),
        "{refusal}"
    );
    assert!(refusal.contains("git-object-read"), "{refusal}");

    // and the set this build DOES run links, so the gate is a gate and not a
    // wall.
    let current = workspace_config::read_module_artifact(&fixtures(), "forge")
        .unwrap()
        .encode();
    assert!(noded::compose::load("forge", &current).is_ok());
}

/// a code source that answers EVERY hash with one fixed component's bytes.
/// a `DirCodeSource` keys itself by what it hashed, so it cannot lie by
/// construction — the composer's re-hash needs a source that can.
struct LiarSource(PathBuf);

#[async_trait::async_trait(?Send)]
impl host::CodeSource for LiarSource {
    async fn fetch(&self, _code_hash: &[u8]) -> Option<Vec<u8>> {
        std::fs::read(&self.0).ok()
    }

    fn origin(&self) -> &'static str {
        "test_liar"
    }
}

/// a code source is a lookup, not a guarantee: bytes that do not hash to the
/// genesis entry never seat, or the running code and the modules registry
/// would silently disagree.
#[test]
fn a_code_source_whose_bytes_miss_the_hash_is_refused() {
    run(|context, dir| {
        Box::pin(async move {
            let (_, by_id) = DirCodeSource::open(&fixtures(), &["acl"]).unwrap();
            // the liar answers acl's hash with governance's component.
            let liar = LiarSource(fixtures().join("governance.component.wasm"));
            let substrates = substrates(&dir);
            let mut stores = qmdb_stores(&context);
            let Err(err) = compose(
                &liar,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Genesis {
                    validators: &[],
                    bundle: &by_id,
                },
            )
            .await
            else {
                panic!("mismatched code bytes composed");
            };
            assert!(
                err.contains("acl") && err.contains("do not match"),
                "the mismatch is refused by module name: {err}"
            );
        })
    });
}

/// Supported engines are available to every valid tenant; host-provided
/// configuration keys remain an explicit capability contract.
#[test]
fn supported_storage_is_not_restricted_to_founding_module_names() {
    for backing in [Backing::Odb, Backing::Git] {
        let shape = Shape {
            backing,
            config: Vec::new(),
            committed_queries: true,
        };
        check_realizable("kanban", &shape).expect("supported engine for a new module");
        assert!(check_realizable("../escape", &shape).is_err());
    }

    let unknown_key = Shape {
        backing: Backing::Store,
        config: vec!["tenant".into()],
        committed_queries: false,
    };
    let err = check_realizable("kanban", &unknown_key).unwrap_err();
    assert!(
        err.contains("kanban") && err.contains("tenant"),
        "an unbound config key names the module and the key: {err}"
    );
    let bound = Shape {
        backing: Backing::Store,
        config: vec![
            sdk::genesis_config::INVITE.into(),
            sdk::genesis_config::CHAIN_ID.into(),
        ],
        committed_queries: true,
    };
    check_realizable("kanban", &bound).expect("the network binds both keys");
}

/// Post-genesis admission uses the same factory as genesis for maps and
/// tenant-scoped object storage, including module ids unknown to the host.
#[test]
fn admissions_build_through_the_one_wasm_path() {
    run(|context, dir| {
        Box::pin(async move {
            let hello = std::fs::read(fixtures().join("hello.component.wasm")).unwrap();
            let object = object_component();
            let substrates = substrates(&dir);
            let admissions = Admissions::new(&context, &substrates, &BINDINGS);
            let admitted = host::ModuleFactory::instantiate(
                &admissions,
                "hello",
                &module_artifact::Artifact::module(hello).encode(),
            )
            .await
            .expect("a map-declared component admits over a fresh map");
            assert_eq!(admitted.id(), "hello");
            let admitted = host::ModuleFactory::instantiate(
                &admissions,
                "kanban",
                &module_artifact::Artifact::module(object).encode(),
            )
            .await
            .expect("a new tenant can use supported object storage");
            assert_eq!(admitted.id(), "kanban");
            assert_eq!(
                substrates.path("kanban").unwrap(),
                dir.join("module-storage/kanban")
            );
            assert!(substrates.path("../escape").is_err());
            // and the refusals that keep the boundary fail-closed. This factory
            // is only ever asked about an entry committed `Kind::Module`, so a
            // component from some other world under a module id is an Err that
            // stalls that id — never an answer that would let one binary seat
            // what another passes over.
            let netstack = std::fs::read(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../networking/netstack-machine/component.wasm"),
            )
            .unwrap();
            let seated = host::ModuleFactory::instantiate(
                &admissions,
                "netstack",
                &module_artifact::Artifact::module(netstack).encode(),
            )
            .await;
            let Err(sdk::Error::Module { reason, .. }) = seated else {
                panic!("a foreign-world component under a module id is a module refusal");
            };
            assert_eq!(reason, "module_seat");
            // and the same refusal one step earlier: bytes carrying no artifact
            // frame at all.
            let seated =
                host::ModuleFactory::instantiate(&admissions, "raw", b"\0asm\x01\0\0\0").await;
            let Err(sdk::Error::Module { reason, .. }) = seated else {
                panic!("unframed bytes under a module id are a module refusal");
            };
            assert_eq!(reason, "artifact_frame_absent");
        })
    });
}

/// the question a validator answers before it signals an admission ready: the
/// admission itself, run to the end over scratch state. Every module the
/// kernel fixture set carries starts there, whatever backing it declares — a
/// refusal here would keep a sound module from ever activating — and a guest
/// that cannot start is refused in the admission's own words.
#[test]
fn an_admission_check_starts_the_module_over_scratch_state() {
    run(|context, dir| {
        Box::pin(async move {
            let admissions = Admissions::new(&context, &substrates(&dir), &BINDINGS);
            for entry in std::fs::read_dir(fixtures()).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_str().unwrap();
                let id = name.strip_suffix(".component.wasm").unwrap();
                let bytes = module_artifact::Artifact::module(std::fs::read(&path).unwrap());
                host::ModuleFactory::check(&admissions, id, &bytes.encode())
                    .unwrap_or_else(|refusal| panic!("{id} starts: {refusal}"));
            }

            let stale = workspace_config::read_module_artifact(&stale_host_wit(), "forge")
                .unwrap()
                .encode();
            let refusal = host::ModuleFactory::check(&admissions, "forge", &stale)
                .expect_err("a guest that cannot start is not ready");
            assert!(
                refusal.to_string().contains("forge component loads"),
                "the admission's own words: {refusal}"
            );
        })
    });
}

/// an ODB-BACKED tenant reads its network's chain id through the same
/// `sdk::genesis_config` seam a Map/Store tenant does — the kernel half of
/// #1773: `compose::wasm_module` used to skip the `Backing::Odb` arm
/// entirely, so a component like forge (or this fixture, wearing files' odb
/// substrate) never saw a `__config` record and could not learn its chain id.
#[test]
fn an_odb_backed_module_reads_its_chain_id_from_genesis_config() {
    run(|context, dir| {
        Box::pin(async move {
            let object = object_component();
            let substrates = substrates(&dir);
            let mut stores = qmdb_stores(&context);
            let bindings = Bindings {
                invite: b"t",
                chain_id: "net#odb-1773",
                time_unit: sdk::genesis_config::TimeUnit::Height,
            };
            let mut module = wasm_module(
                "files",
                &module_artifact::Artifact::module(object).encode(),
                &mut stores,
                &substrates,
                &bindings,
                Start::Fresh {
                    parameters: &sdk::genesis_config::encode_config(&[]),
                },
            )
            .await
            .expect("an odb-declared component wraps over the files substrate");

            let mut ctx = TestCtx::at_height(0);
            let matching = sdk::Msg {
                target: "files".into(),
                payload: [b"c".as_slice(), bindings.chain_id.as_bytes()].concat(),
            };
            module
                .execute(&mut ctx, &matching)
                .await
                .expect("the guest read chain_id straight out of __config");

            let mismatched = sdk::Msg {
                target: "files".into(),
                payload: [b"c".as_slice(), b"some-other-chain".as_slice()].concat(),
            };
            module
                .execute(&mut ctx, &mismatched)
                .await
                .expect_err("a wrong expectation against the same __config is rejected");
        })
    });
}

struct ArtifactSource(std::collections::BTreeMap<Vec<u8>, Vec<u8>>);

impl ArtifactSource {
    fn add(&mut self, artifact: impl Into<module_artifact::Artifact>) -> [u8; 32] {
        let artifact = artifact.into();
        let hash = artifact.hash();
        self.0.insert(hash.to_vec(), artifact.encode());
        hash
    }
}

#[async_trait::async_trait(?Send)]
impl host::CodeSource for ArtifactSource {
    async fn fetch(&self, hash: &[u8]) -> Option<Vec<u8>> {
        self.0.get(hash).cloned()
    }

    fn origin(&self) -> &'static str {
        "test_artifacts"
    }
}

async fn registry_op(
    host: &mut host::Host,
    height: u64,
    origin: sdk::Origin,
    op: modules::ModulesMsg,
) {
    host.submit_at(
        host::BlockContext {
            height,
            consensus_time: height,
            origin,
        },
        sdk::Msg {
            target: "modules".into(),
            payload: modules::encode_msg(&op),
        },
    )
    .await
    .unwrap();
}

async fn ready(host: &mut host::Host, height: u64, member: &[u8], id: &str, hash: [u8; 32]) {
    registry_op(
        host,
        height,
        sdk::Origin::External(member.to_vec()),
        modules::ModulesMsg::SwapReady {
            name: format!("deploy-{id}"),
            module_id: id.into(),
            code_hash: hash.to_vec(),
        },
    )
    .await;
}

async fn schedule_swap(host: &mut host::Host, height: u64, id: &str, hash: [u8; 32], at: u64) {
    registry_op(
        host,
        height,
        sdk::Origin::System,
        modules::ModulesMsg::ScheduleSwap {
            name: format!("deploy-{id}"),
            module_id: id.into(),
            activation_height: at,
            code_hash: hash.to_vec(),
        },
    )
    .await;
}

/// a founding `<id>.view.wasm` with no core is a `Kind::View` registry entry:
/// the genesis seeds it (the registry lists it, kind `view`, active on its
/// hash), no module seats under its id, the boundary leaves it alone at every
/// height, a reopen off the seated set does not miss it, and a live
/// registration of a second view latches on the view frame alone and never
/// asks the admission factory for a core.
#[test]
fn a_view_entry_composes_no_module_and_the_boundary_leaves_it_alone() {
    use commonware_cryptography::Signer as _;
    use module_artifact::{Artifact, ViewArtifact};
    use sdk::Origin;
    run(|context, dir| {
        Box::pin(async move {
            let member = commonware_cryptography::ed25519::PrivateKey::from_seed(1)
                .public_key()
                .as_ref()
                .to_vec();
            let mut source = ArtifactSource(Default::default());
            let mut codes = std::collections::BTreeMap::new();
            for id in ["modules", "valset"] {
                let bytes = std::fs::read(fixtures().join(format!("{id}.component.wasm"))).unwrap();
                codes.insert(id.to_string(), source.add(Artifact::module(bytes)));
            }
            let home = source.add(Artifact::View(ViewArtifact {
                component: view_component(),
                assets: [("icons/tab.svg".to_owned(), b"<svg/>".to_vec())].into(),
            }));
            codes.insert("home".into(), home);
            assert_eq!(
                noded::compose::genesis_seeds(&[noded::compose::Founding {
                    id: "home".into(),
                    hash: home,
                    kind: modules::Kind::View,
                    lanes: Vec::new(),
                }])["home"],
                modules::Seed {
                    kind: modules::Kind::View,
                    code_hash: home.to_vec(),
                    lanes: Vec::new()
                }
            );

            let substrates = substrates(&dir);
            let mut stores = qmdb_stores(&context);
            let mut host = compose(
                &source,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Genesis {
                    validators: std::slice::from_ref(&member),
                    bundle: &codes,
                },
            )
            .await
            .unwrap();
            host.set_module_factory(Box::new(Admissions::new(&context, &substrates, &BINDINGS)));
            assert!(host.module_root("home").is_none(), "a view seats no module");
            let ids: Vec<String> = host.module_roots().into_iter().map(|(id, _)| id).collect();
            assert_eq!(ids, ["modules", "valset"]);
            let status = host.module_status().await.unwrap().unwrap();
            let entry = status.iter().find(|m| m.module_id == "home").unwrap();
            assert_eq!(entry.kind, modules::Kind::View);
            assert_eq!(entry.active_code_hash, home.to_vec());
            assert_eq!(entry.history.len(), 1, "seeded active at genesis");

            // every boundary: nothing to realize for the view, no factory
            // call, the roster unchanged.
            let root = host.root_hash();
            for height in 1..=3 {
                host.realize_module_swaps(height, &source).await.unwrap();
                registry_op(
                    &mut host,
                    height,
                    Origin::System,
                    modules::ModulesMsg::Advance,
                )
                .await;
            }
            assert!(host.module_root("home").is_none());
            assert_eq!(host.root_hash(), root);

            // a second view registered live: readiness is the view ABI alone
            // (the drain's verdict), the swap latches at R = n and advances
            // with no core ever asked of the factory.
            let dashboard = source.add(Artifact::View(ViewArtifact {
                component: view_component(),
                assets: [("icons/tab.svg".to_owned(), b"<svg>2</svg>".to_vec())].into(),
            }));
            let index = indexer::IndexStore::open_bare(dir.join("index"), &["modules"]).unwrap();
            noded::compose::validate_deployment(
                "dashboard",
                modules::Kind::View,
                &source.0[&dashboard.to_vec()],
                &index,
            )
            .unwrap();
            registry_op(
                &mut host,
                4,
                Origin::System,
                modules::ModulesMsg::ScheduleRegister {
                    name: "deploy-dashboard".into(),
                    module_id: "dashboard".into(),
                    kind: modules::Kind::View,
                    activation_height: 10,
                    code_hash: dashboard.to_vec(),
                    lanes: Vec::new(),
                },
            )
            .await;
            ready(&mut host, 5, &member, "dashboard", dashboard).await;
            host.realize_module_swaps(10, &source).await.unwrap();
            registry_op(&mut host, 10, Origin::System, modules::ModulesMsg::Advance).await;
            assert!(host.module_root("dashboard").is_none());
            let status = host.module_status().await.unwrap().unwrap();
            let entry = status.iter().find(|m| m.module_id == "dashboard").unwrap();
            assert_eq!(entry.kind, modules::Kind::View);
            assert_eq!(entry.active_code_hash, dashboard.to_vec());
            assert!(entry.pending.is_none(), "the view activated at its height");

            // reopen off the SEATED set (what a checkpoint records): the
            // registry still lists both views, and the root is the same.
            let root = host.root_hash();
            drop(host);
            let mut snapshots =
                |_: &str, _: Backing| -> SnapshotFut<'_> { Box::pin(async { Ok(None) }) };
            codes.remove("home");
            let reopened = compose(
                &source,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Reopen {
                    height: 10,
                    codes: &codes,
                    snapshots: &mut snapshots,
                },
            )
            .await
            .unwrap();
            assert_eq!(reopened.root_hash(), root);
            assert!(reopened.module_root("home").is_none());
            assert!(reopened.module_root("dashboard").is_none());
            let status = reopened.module_status().await.unwrap().unwrap();
            let views: Vec<&str> = status
                .iter()
                .filter(|m| m.kind == modules::Kind::View)
                .map(|m| m.module_id.as_str())
                .collect();
            assert_eq!(views, ["dashboard", "home"]);
        })
    });
}

/// Both registries are actual Wasm. A live admission carries its mapper, an
/// update can remove that mapper, and the registry can replace ITSELF. Reopen
/// uses authenticated deployment hashes, including the registry's new code.
#[test]
fn wasm_registry_admits_a_mapper_removes_it_and_reopens_after_self_swap() {
    use commonware_cryptography::Signer as _;
    use module_artifact::{Artifact, ModuleArtifact};
    use sdk::Origin;
    run(|context, dir| {
        Box::pin(async move {
            let member = commonware_cryptography::ed25519::PrivateKey::from_seed(1)
                .public_key()
                .as_ref()
                .to_vec();
            let mut source = ArtifactSource(Default::default());
            let mut codes = std::collections::BTreeMap::new();
            for id in ["modules", "valset", "identity", "attribution"] {
                let bytes = std::fs::read(fixtures().join(format!("{id}.component.wasm"))).unwrap();
                codes.insert(id.to_string(), source.add(Artifact::module(bytes)));
            }
            let pages = std::fs::read(fixtures().join("pages.component.wasm")).unwrap();
            let mapper = std::fs::read(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../modules/apps/pages/index.wasm"),
            )
            .unwrap();
            let indexed = source.add(ModuleArtifact {
                view: None,
                component: pages.clone(),
                index: Some(mapper),
                lanes: Vec::new(),
            });
            let bare = source.add(Artifact::module(pages));
            assert_ne!(indexed, bare, "mapper removal is a different deployment");
            let mut replacement = std::fs::read(fixtures().join("modules.component.wasm")).unwrap();
            // A valid custom section changes the deployment identity while keeping
            // the registry ABI and storage layout, so the replacement can reopen it.
            replacement.extend_from_slice(&[0, 6, 5, b'p', b'r', b'o', b'o', b'f']);
            let registry_replacement = source.add(Artifact::module(replacement));
            assert_ne!(registry_replacement, codes["modules"]);

            let substrates = substrates(&dir);
            let mut stores = qmdb_stores(&context);
            let mut host = compose(
                &source,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Genesis {
                    validators: std::slice::from_ref(&member),
                    bundle: &codes,
                },
            )
            .await
            .unwrap();
            host.set_module_factory(Box::new(Admissions::new(&context, &substrates, &BINDINGS)));
            let initial_root = host.root_hash();
            for (fixture, reason) in [("hello", "backing"), ("identity", "configuration")] {
                let bytes =
                    std::fs::read(fixtures().join(format!("{fixture}.component.wasm"))).unwrap();
                let deployment = Artifact::module(bytes).encode();
                let error = host
                    .check_module_replacement("valset", &deployment)
                    .unwrap_err();
                assert!(error.to_string().contains(reason), "{error}");
                assert_eq!(
                    host.root_hash(),
                    initial_root,
                    "a preflight changes no running module"
                );
            }
            host.check_module_replacement("valset", &source.0[&codes["valset"].to_vec()])
                .unwrap();
            assert_eq!(host.root_hash(), initial_root);

            let index =
                indexer::IndexStore::open_bare(dir.join("index"), &["modules", "valset"]).unwrap();
            noded::converge_host_modules(&index, &host).unwrap();
            noded::compose::validate_deployment(
                "pages",
                modules::Kind::Module,
                &source.0[&indexed.to_vec()],
                &index,
            )
            .unwrap();
            let Artifact::Module(mut invalid_mapper) =
                Artifact::decode(&source.0[&indexed.to_vec()]).unwrap()
            else {
                panic!("the indexed deployment is a module");
            };
            invalid_mapper.index = Some(b"not wasm".to_vec());
            assert!(
                noded::compose::validate_deployment(
                    "pages",
                    modules::Kind::Module,
                    &Artifact::Module(invalid_mapper).encode(),
                    &index
                )
                .is_err()
            );

            registry_op(
                &mut host,
                1,
                Origin::System,
                modules::ModulesMsg::ScheduleRegister {
                    name: "deploy-pages".into(),
                    module_id: "pages".into(),
                    kind: modules::Kind::Module,
                    activation_height: 10,
                    code_hash: indexed.to_vec(),
                    lanes: Vec::new(),
                },
            )
            .await;
            ready(&mut host, 2, &member, "pages", indexed).await;
            host.realize_module_swaps(9, &source).await.unwrap();
            assert!(host.module_root("pages").is_none());
            assert!(!index.module_ids().iter().any(|id| id == "pages"));
            host.realize_module_swaps(10, &source).await.unwrap();
            registry_op(&mut host, 10, Origin::System, modules::ModulesMsg::Advance).await;
            noded::converge_host_modules(&index, &host).unwrap();
            assert_eq!(host.module_code_hash("pages").unwrap(), indexed);
            let out = host
                .submit_at(
                    host::BlockContext {
                        height: 11,
                        consensus_time: 11,
                        origin: Origin::External(member.clone()),
                    },
                    sdk::Msg {
                        target: "pages".into(),
                        payload: br#"{"create_page":{"page_id":"first","title":"First"}}"#.to_vec(),
                    },
                )
                .await
                .unwrap();
            noded::projection::apply_block_to_index(&index, 11, 11, None, &out.dispatches, &host);
            index.wait_folds_drained().unwrap();
            let query = br#"{"list_pages":{}}"#;
            let view: serde_json::Value =
                serde_json::from_slice(&index.view("pages", query).unwrap()).unwrap();
            assert_eq!(
                view["pages"]["pages"].as_array().unwrap().len(),
                1,
                "{view}"
            );
            assert!(
                !index
                    .scan("pages", b"page/", None, 10)
                    .unwrap()
                    .entries
                    .is_empty()
            );
            let state = host.module_root("pages").unwrap();
            schedule_swap(&mut host, 12, "pages", bare, 20).await;
            ready(&mut host, 13, &member, "pages", bare).await;
            host.realize_module_swaps(19, &source).await.unwrap();
            noded::converge_host_modules(&index, &host).unwrap();
            assert!(index.view("pages", query).is_ok());
            let before = host.root_hash();
            host.realize_module_swaps(20, &source).await.unwrap();
            assert_eq!(
                host.module_root("pages").unwrap(),
                state,
                "code replacement preserves state"
            );
            assert_ne!(
                host.root_hash(),
                before,
                "global root authenticates the deployment"
            );
            registry_op(&mut host, 20, Origin::System, modules::ModulesMsg::Advance).await;
            noded::converge_host_modules(&index, &host).unwrap();
            assert!(matches!(
                index.view("pages", query),
                Err(indexer::Error::ViewUnsupported)
            ));
            assert!(
                index
                    .scan("pages", b"page/", None, 10)
                    .unwrap()
                    .entries
                    .is_empty()
            );
            assert!(
                !index
                    .scan("pages", indexer::OP_PREFIX.as_bytes(), None, 10)
                    .unwrap()
                    .entries
                    .is_empty()
            );

            schedule_swap(&mut host, 21, "modules", registry_replacement, 30).await;
            ready(&mut host, 22, &member, "modules", registry_replacement).await;
            host.realize_module_swaps(30, &source).await.unwrap();
            registry_op(&mut host, 30, Origin::System, modules::ModulesMsg::Advance).await;
            assert_eq!(
                host.module_code_hash("modules").unwrap(),
                registry_replacement
            );
            let status = host.module_status().await.unwrap().unwrap();
            assert_eq!(
                status
                    .iter()
                    .find(|m| m.module_id == "modules")
                    .unwrap()
                    .active_code_hash,
                registry_replacement
            );
            codes.insert("modules".into(), registry_replacement);
            codes.insert("pages".into(), bare);
            let root = host.root_hash();
            drop(host);
            let mut snapshots =
                |_: &str, _: Backing| -> SnapshotFut<'_> { Box::pin(async { Ok(None) }) };
            let reopened = compose(
                &source,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Reopen {
                    height: 30,
                    codes: &codes,
                    snapshots: &mut snapshots,
                },
            )
            .await
            .unwrap();
            assert_eq!(reopened.root_hash(), root);
            assert_eq!(
                reopened.module_code_hash("modules").unwrap(),
                registry_replacement
            );
            noded::converge_host_modules(&index, &reopened).unwrap();
            assert!(matches!(
                index.view("pages", query),
                Err(indexer::Error::ViewUnsupported)
            ));
        })
    });
}

fn view_component() -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/view.component.wasm"),
    )
    .unwrap()
}

fn view_deployment(component: Vec<u8>) -> module_artifact::ModuleArtifact {
    module_artifact::ModuleArtifact {
        view: Some(module_artifact::ViewArtifact {
            component,
            assets: Default::default(),
        }),
        ..module_artifact::ModuleArtifact::component(
            std::fs::read(fixtures().join("pages.component.wasm")).unwrap(),
        )
    }
}

fn encode(artifact: &module_artifact::ModuleArtifact) -> Vec<u8> {
    module_artifact::Artifact::Module(artifact.clone()).encode()
}

#[test]
fn deployment_readiness_rejects_invalid_view_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexer::IndexStore::open_bare(dir.path(), &["pages"]).unwrap();
    let mut view = view_component();
    let marker = b"ducktape.view.manifest.v1";
    // The guest retains the text in data as well as its custom section.
    // Corrupt every copy so the actual metadata, not just data, is invalid.
    let offsets: Vec<_> = view
        .windows(marker.len())
        .enumerate()
        .filter_map(|(offset, bytes)| (bytes == marker).then_some(offset))
        .collect();
    assert!(!offsets.is_empty());
    for offset in offsets {
        view[offset] = b'x';
    }
    assert!(view_wire::manifest::read_manifest(&view).is_none());
    let error = noded::compose::validate_deployment(
        "pages",
        modules::Kind::Module,
        &encode(&view_deployment(view)),
        &index,
    )
    .expect_err("invalid view manifest must refuse readiness");
    assert!(error.contains("view manifest"), "{error}");
}

#[test]
fn deployment_readiness_rejects_invalid_view_abi() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexer::IndexStore::open_bare(dir.path(), &["pages"]).unwrap();
    // A valid component and strict manifest, but init has the wrong type.
    let mut view = wat::parse_str(
        r#"(component
        (core module $m (func (export "init") unreachable))
        (core instance $i (instantiate $m))
        (func (export "init") (canon lift (core func $i "init"))))"#,
    )
    .unwrap();
    append_manifest(&mut view);
    let error = noded::compose::validate_deployment(
        "pages",
        modules::Kind::Module,
        &encode(&view_deployment(view)),
        &index,
    )
    .expect_err("wrong view export type must refuse readiness");
    assert!(
        error.contains("view ABI") && error.contains("init"),
        "{error}"
    );
}

fn append_manifest(view: &mut Vec<u8>) {
    let name = b"ducktape.view.manifest";
    let text = format!(
        "ducktape.view.manifest.v1\nTest\n\n\nnone\n{}",
        view_wire::WIRE_EPOCH
    );
    let text = text.as_bytes();
    view.extend_from_slice(&[0, (1 + name.len() + text.len()) as u8, name.len() as u8]);
    view.extend_from_slice(name);
    view.extend_from_slice(text);
}

#[test]
fn deployment_readiness_accepts_actual_view() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexer::IndexStore::open_bare(dir.path(), &["pages"]).unwrap();
    noded::compose::validate_deployment(
        "pages",
        modules::Kind::Module,
        &encode(&view_deployment(view_component())),
        &index,
    )
    .unwrap();
}

/// a `Kind::View` entry is ready on the view ABI alone: no core to compile,
/// no shape to realize. the frame's tag must be the entry's kind either way.
#[test]
fn a_view_entry_is_ready_on_the_view_alone_and_the_tag_must_match_the_kind() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexer::IndexStore::open_bare(dir.path(), &["pages"]).unwrap();
    let view_frame = module_artifact::Artifact::View(module_artifact::ViewArtifact {
        component: view_component(),
        assets: [("icons/tab.svg".to_owned(), b"<svg/>".to_vec())].into(),
    })
    .encode();
    assert_eq!(
        noded::compose::artifact_kind(&view_frame).unwrap(),
        modules::Kind::View
    );
    noded::compose::validate_deployment("home", modules::Kind::View, &view_frame, &index).unwrap();
    let error =
        noded::compose::validate_deployment("home", modules::Kind::Module, &view_frame, &index)
            .unwrap_err();
    assert!(error.starts_with("artifact_kind_mismatch"), "{error}");
    let module_frame = encode(&view_deployment(view_component()));
    assert_eq!(
        noded::compose::artifact_kind(&module_frame).unwrap(),
        modules::Kind::Module
    );
    let error =
        noded::compose::validate_deployment("pages", modules::Kind::View, &module_frame, &index)
            .unwrap_err();
    assert!(error.starts_with("artifact_kind_mismatch"), "{error}");
    // a view that is no view refuses a view entry the same way it refuses a
    // module's embedded one
    let broken = module_artifact::Artifact::View(module_artifact::ViewArtifact {
        component: b"not a component".to_vec(),
        assets: Default::default(),
    })
    .encode();
    let error = noded::compose::validate_deployment("home", modules::Kind::View, &broken, &index)
        .unwrap_err();
    assert!(error.contains("view manifest"), "{error}");
}

/// a `Kind::Plane` entry is a hash this boundary holds and nothing more: the
/// artifact belongs to the node plane that owns it, so static deployment
/// validation does not decode it. The owner supplies the live restore proof
/// before a validator signals readiness.
#[test]
fn a_plane_entry_is_ready_without_decoding_its_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexer::IndexStore::open_bare(dir.path(), &["pages"]).unwrap();
    let not_a_frame = b"\0asm\x01\0\0\0 a netstack guest, not a deployment frame";
    module_artifact::ArtifactRef::decode(not_a_frame)
        .expect_err("fixture must not be a decodable deployment frame");
    noded::compose::validate_deployment("netstack", modules::Kind::Plane, not_a_frame, &index)
        .unwrap();
}

#[test]
fn deployment_readiness_does_not_instantiate_view() {
    let dir = tempfile::tempdir().unwrap();
    let index = indexer::IndexStore::open_bare(dir.path(), &["pages"]).unwrap();
    // Every function traps, including the core start. A valid ABI must pass
    // readiness, but ANY component instantiation must fail before init.
    let mut view = wat::parse_str(r#"(component
        (core module $m
            (memory (export "memory") 1)
            (func $start unreachable)
            (start $start)
            (func (export "realloc") (param i32 i32 i32 i32) (result i32) unreachable)
            (func (export "init") (param i32) unreachable)
            (func (export "tick") (param i32 i32) (result i32) unreachable)
            (func (export "snapshot") (result i32) unreachable)
            (func (export "restore") (param i32 i32 i32) (result i32) unreachable))
        (core instance $i (instantiate $m))
        (func (export "init") (param "macos" bool)
            (canon lift (core func $i "init")))
        (func (export "tick") (param "events" (list u8)) (result (list u8))
            (canon lift (core func $i "tick") (memory $i "memory") (realloc (func $i "realloc"))))
        (func (export "snapshot") (result (result (list u8) (error string)))
            (canon lift (core func $i "snapshot") (memory $i "memory") (realloc (func $i "realloc"))))
        (func (export "restore") (param "state" (list u8)) (param "macos" bool)
            (result (result (error string)))
            (canon lift (core func $i "restore") (memory $i "memory") (realloc (func $i "realloc")))))"#).unwrap();
    append_manifest(&mut view);
    noded::compose::validate_deployment(
        "pages",
        modules::Kind::Module,
        &encode(&view_deployment(view.clone())),
        &index,
    )
    .expect("static view readiness must not execute the trapping start");
    // Prove the fixture's trap is reached on real instantiation; a passing
    // readiness assertion alone would not establish this counterexample.
    let engine = wasmtime::Engine::default();
    let component = wasmtime::component::Component::from_binary(&engine, &view).unwrap();
    let linker = wasmtime::component::Linker::<()>::new(&engine);
    let mut store = wasmtime::Store::new(&engine, ());
    let error = linker.instantiate(&mut store, &component).unwrap_err();
    assert!(format!("{error:#}").contains("unreachable"), "{error:#}");
}

async fn assert_deployment(
    host: &host::Host,
    source: &ArtifactSource,
    hash: [u8; 32],
    expected: &module_artifact::ModuleArtifact,
) {
    assert_eq!(host.module_code_hash("pages").unwrap(), hash);
    let bytes = noded::compose::fetch_code(source, "pages", &hash)
        .await
        .unwrap();
    assert_eq!(
        module_artifact::Artifact::decode(&bytes).unwrap(),
        module_artifact::Artifact::Module(expected.clone())
    );
}

#[test]
fn wasm_registry_activates_view_assets_and_reopens_after_view_removal() {
    use commonware_cryptography::Signer as _;
    use module_artifact::{Artifact, ModuleArtifact};
    use sdk::Origin;
    run(|context, dir| {
        Box::pin(async move {
            let member = commonware_cryptography::ed25519::PrivateKey::from_seed(1)
                .public_key()
                .as_ref()
                .to_vec();
            let mut source = ArtifactSource(Default::default());
            let mut codes = std::collections::BTreeMap::new();
            for id in ["modules", "valset", "identity", "attribution"] {
                let bytes = std::fs::read(fixtures().join(format!("{id}.component.wasm"))).unwrap();
                codes.insert(id.to_string(), source.add(Artifact::module(bytes)));
            }
            let mut first = view_deployment(view_component());
            first.index = Some(
                std::fs::read(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("../modules/apps/pages/index.wasm"),
                )
                .unwrap(),
            );
            first.view.as_mut().unwrap().assets.insert(
                "images/proof.svg".into(),
                b"<svg xmlns='http://www.w3.org/2000/svg'><title>first</title></svg>".to_vec(),
            );
            let mut changed_view = first.clone();
            changed_view
                .view
                .as_mut()
                .unwrap()
                .component
                .extend_from_slice(&[0, 6, 5, b'p', b'r', b'o', b'o', b'f']);
            let mut changed_asset = changed_view.clone();
            changed_asset.view.as_mut().unwrap().assets.insert(
                "images/proof.svg".into(),
                b"<svg xmlns='http://www.w3.org/2000/svg'><title>second</title></svg>".to_vec(),
            );
            let removed = ModuleArtifact {
                view: None,
                ..changed_asset.clone()
            };
            let deployments = [first, changed_view, changed_asset, removed];
            let hashes = deployments
                .each_ref()
                .map(|artifact| source.add(artifact.clone()));
            assert_eq!(
                hashes
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                4,
                "view changes, asset changes, and view removal each change the deployment hash"
            );
            let substrates = substrates(&dir);
            let mut stores = qmdb_stores(&context);
            let mut host = compose(
                &source,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Genesis {
                    validators: std::slice::from_ref(&member),
                    bundle: &codes,
                },
            )
            .await
            .unwrap();
            host.set_module_factory(Box::new(Admissions::new(&context, &substrates, &BINDINGS)));
            let index =
                indexer::IndexStore::open_bare(dir.join("index"), &["modules", "valset"]).unwrap();
            for deployment in &deployments {
                noded::compose::validate_deployment(
                    "pages",
                    modules::Kind::Module,
                    &encode(deployment),
                    &index,
                )
                .unwrap();
                assert_eq!(deployment.component, deployments[0].component);
                assert_eq!(deployment.index, deployments[0].index);
            }
            registry_op(
                &mut host,
                1,
                Origin::System,
                modules::ModulesMsg::ScheduleRegister {
                    name: "deploy-pages".into(),
                    module_id: "pages".into(),
                    kind: modules::Kind::Module,
                    activation_height: 10,
                    code_hash: hashes[0].to_vec(),
                    lanes: Vec::new(),
                },
            )
            .await;
            ready(&mut host, 2, &member, "pages", hashes[0]).await;
            host.realize_module_swaps(9, &source).await.unwrap();
            assert!(host.module_root("pages").is_none());
            host.realize_module_swaps(10, &source).await.unwrap();
            registry_op(&mut host, 10, Origin::System, modules::ModulesMsg::Advance).await;
            assert_deployment(&host, &source, hashes[0], &deployments[0]).await;
            noded::converge_host_modules(&index, &host).unwrap();
            let out = host
                .submit_at(
                    host::BlockContext {
                        height: 11,
                        consensus_time: 11,
                        origin: Origin::External(member.clone()),
                    },
                    sdk::Msg {
                        target: "pages".into(),
                        payload: br#"{"create_page":{"page_id":"first","title":"First"}}"#.to_vec(),
                    },
                )
                .await
                .unwrap();
            noded::projection::apply_block_to_index(&index, 11, 11, None, &out.dispatches, &host);
            index.wait_folds_drained().unwrap();
            let state = host.module_root("pages").unwrap();
            let query = br#"{"list_pages":{}}"#;
            let indexed_page = index.view("pages", query).unwrap();
            let parsed: serde_json::Value = serde_json::from_slice(&indexed_page).unwrap();
            assert_eq!(parsed["pages"]["pages"].as_array().unwrap().len(), 1);
            for (next, at) in [(1, 20), (2, 30), (3, 40)] {
                schedule_swap(&mut host, at - 8, "pages", hashes[next], at).await;
                ready(&mut host, at - 7, &member, "pages", hashes[next]).await;
                host.realize_module_swaps(at - 1, &source).await.unwrap();
                assert_deployment(&host, &source, hashes[next - 1], &deployments[next - 1]).await;
                let before = host.root_hash();
                host.realize_module_swaps(at, &source).await.unwrap();
                assert_deployment(&host, &source, hashes[next], &deployments[next]).await;
                assert_eq!(
                    host.module_root("pages").unwrap(),
                    state,
                    "view swaps preserve consensus state"
                );
                assert_ne!(
                    host.root_hash(),
                    before,
                    "global root authenticates the view deployment"
                );
                registry_op(&mut host, at, Origin::System, modules::ModulesMsg::Advance).await;
                noded::converge_host_modules(&index, &host).unwrap();
                index.wait_folds_drained().unwrap();
                assert_eq!(
                    index.view("pages", query).unwrap(),
                    indexed_page,
                    "view removal preserves the mapper"
                );
            }
            let status = host.module_status().await.unwrap().unwrap();
            let pages = status
                .iter()
                .find(|entry| entry.module_id == "pages")
                .unwrap();
            for (height, selected) in [(19, 0), (20, 1), (29, 1), (30, 2), (39, 2), (40, 3)] {
                assert_eq!(modules::code_at(pages, height).unwrap(), hashes[selected]);
            }
            codes.insert("pages".into(), hashes[3]);
            let root = host.root_hash();
            drop(host);
            let mut snapshots =
                |_: &str, _: Backing| -> SnapshotFut<'_> { Box::pin(async { Ok(None) }) };
            let reopened = compose(
                &source,
                &mut stores,
                &substrates,
                &BINDINGS,
                Boot::Reopen {
                    height: 40,
                    codes: &codes,
                    snapshots: &mut snapshots,
                },
            )
            .await
            .unwrap();
            assert_eq!(reopened.root_hash(), root);
            assert_eq!(reopened.module_root("pages").unwrap(), state);
            assert_deployment(&reopened, &source, hashes[3], &deployments[3]).await;
            noded::converge_host_modules(&index, &reopened).unwrap();
            index.wait_folds_drained().unwrap();
            assert_eq!(index.view("pages", query).unwrap(), indexed_page);
        })
    });
}
