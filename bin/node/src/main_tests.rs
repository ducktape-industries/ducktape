use super::*;
use commonware_cryptography::ed25519;
use directory::{DirMsg, Directory, encode_msg};
use host::Host;
use recovery::Manifest;
use sdk::{Ctx, Error, Module, StateSyncHandle};
use std::sync::{Arc, Mutex};

#[test]
fn gateway_requires_a_running_node_and_a_real_overlay() {
    let wireguard = Some("127.0.0.1:51820".parse().unwrap());
    assert!(gateway_can_start(false, Some("127.0.0.1:0"), wireguard));
    for allowed in [
        gateway_can_start(true, Some("127.0.0.1:0"), wireguard),
        gateway_can_start(false, None, wireguard),
        gateway_can_start(false, Some("127.0.0.1:0"), None),
    ] {
        assert!(!allowed);
    }
}

fn test_root(byte: u8) -> StateRoot {
    StateRoot([byte; sdk::ROOT_LEN])
}

fn test_me() -> Vec<u8> {
    vec![1u8; 32]
}

fn test_manifest(
    height: u64,
    root_hash: StateRoot,
    floor_cert: Option<Vec<u8>>,
) -> statesync::Manifest {
    statesync::Manifest {
        height,
        root_hash,
        epoch: 0,
        view_base: 0,
        participants: vec![test_me()],
        residents: vec![],
        floor_cert,
        entries: vec![],
        applied_frames: vec![],
        pending_cutover_view: None,
    }
}

fn test_manifest_with_participants(
    height: u64,
    root_hash: StateRoot,
    floor_cert: Option<Vec<u8>>,
    participants: Vec<Vec<u8>>,
) -> statesync::Manifest {
    statesync::Manifest {
        participants,
        ..test_manifest(height, root_hash, floor_cert)
    }
}

fn test_manifest_with_base(
    height: u64,
    view_base: u64,
    root_hash: StateRoot,
    floor_cert: Option<Vec<u8>>,
) -> statesync::Manifest {
    statesync::Manifest {
        view_base,
        ..test_manifest(height, root_hash, floor_cert)
    }
}

fn fresh_directory_host() -> Host {
    Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis")
}

#[derive(Clone, Default)]
struct TestDiskStore(Arc<Mutex<u8>>);

impl TestDiskStore {
    fn get(&self) -> u8 {
        *self.0.lock().expect("test disk store lock")
    }

    fn set(&self, value: u8) {
        *self.0.lock().expect("test disk store lock") = value;
    }
}

struct TestDiskModule {
    store: TestDiskStore,
    staged: Option<u8>,
}

impl TestDiskModule {
    fn new(store: TestDiskStore) -> Self {
        Self {
            store,
            staged: None,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Module for TestDiskModule {
    fn id(&self) -> String {
        "disk".into()
    }

    fn root(&self) -> StateRoot {
        test_root(self.store.get())
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::ResolverBacked {
            backend: "test-disk".into(),
            detail: "test disk module reopens from shared durable state".into(),
        })
    }

    async fn execute(&mut self, ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let value = *msg
            .payload
            .first()
            .ok_or_else(|| Error::module("missing_test_value", "missing test value"))?;
        self.staged = Some(value);
        ctx.emit_msg(Msg {
            target: "mem".into(),
            payload: vec![value],
        });
        Ok(())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(value) = self.staged.take() {
            self.store.set(value);
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

struct TestMemoryModule {
    value: u8,
    staged: Option<u8>,
}

impl TestMemoryModule {
    fn new(value: u8) -> Self {
        Self {
            value,
            staged: None,
        }
    }

    fn install(&mut self, bytes: &[u8], root: StateRoot) -> Result<(), Error> {
        let [value] = bytes else {
            return Err(Error::module(
                "bad_test_snapshot",
                "bad test memory snapshot",
            ));
        };
        if test_root(*value) != root {
            return Err(Error::module(
                "test_root_mismatch",
                "test memory root mismatch",
            ));
        }
        self.value = *value;
        self.staged = None;
        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl Module for TestMemoryModule {
    fn id(&self) -> String {
        "mem".into()
    }

    fn root(&self) -> StateRoot {
        test_root(self.value)
    }

    fn state_sync_handle(&self) -> Result<StateSyncHandle, Error> {
        Ok(StateSyncHandle::SnapshotBytes(vec![self.value]))
    }

    async fn execute(&mut self, _ctx: &mut dyn Ctx, msg: &Msg) -> Result<(), Error> {
        let value = *msg
            .payload
            .first()
            .ok_or_else(|| Error::module("missing_test_value", "missing test value"))?;
        self.staged = Some(value);
        Ok(())
    }

    async fn commit_block(&mut self) -> Result<(), Error> {
        if let Some(value) = self.staged.take() {
            self.value = value;
        }
        Ok(())
    }

    async fn abort_block(&mut self) -> Result<(), Error> {
        self.staged = None;
        Ok(())
    }
}

fn mixed_durability_host(store: TestDiskStore, memory_value: u8) -> Host {
    Host::genesis(vec![
        Box::new(TestDiskModule::new(store)),
        Box::new(TestMemoryModule::new(memory_value)),
    ])
    .expect("mixed host")
}

fn restore_mixed_durability_host(store: TestDiskStore, manifest: &Manifest) -> Host {
    let mut memory = TestMemoryModule::new(0);
    memory
        .install(
            manifest.snapshot("mem").expect("mem snapshot"),
            manifest.root("mem").expect("mem root"),
        )
        .expect("mem install");
    Host::genesis(vec![Box::new(TestDiskModule::new(store)), Box::new(memory)])
        .expect("restored mixed host")
}

fn dir_set(key: &str, value: &str) -> Msg {
    Msg {
        target: "directory".into(),
        payload: encode_msg(&DirMsg::Set {
            key: key.into(),
            value: value.into(),
        }),
    }
}

async fn dir_value(host: &Host, key: &str) -> Option<String> {
    let reply = host
        .query(
            "directory",
            &encode_query(&DirQuery::Get { key: key.into() }),
        )
        .await
        .expect("query");
    match decode_reply(&reply).expect("decode") {
        DirReply::Value(v) => v,
    }
}

#[test]
fn resident_manifest_fetch_retry_stays_resident_and_does_not_reannounce() {
    let retry = joiner_manifest_fetch_retry(
        "9f7bae44",
        true,
        1,
        "server error: no finalized boundary to serve yet",
    )
    .expect("the first failure speaks");

    assert!(
        !retry.announce,
        "a resident should not re-announce the invite after standing is known"
    );
    assert!(
        retry.log_line.contains("[node 9f7bae44] resident:"),
        "post-standing retry should be logged as resident follow noise: {}",
        retry.log_line
    );
    assert!(
        retry
            .log_line
            .contains("no finalized boundary to serve yet"),
        "the source fetch detail should remain visible: {}",
        retry.log_line
    );
    assert!(
        !retry.log_line.contains("redemption not landed") && !retry.log_line.contains("joining:"),
        "post-standing retry must not look like a pending invite: {}",
        retry.log_line
    );
}

#[test]
fn parked_manifest_fetch_retry_keeps_join_announce() {
    let retry = joiner_manifest_fetch_retry("9f7bae44", false, 1, "server error: bouncer rejected")
        .expect("the first failure speaks");

    assert!(retry.announce, "a parked joiner must keep re-announcing");
    assert!(
        retry.log_line.contains("[node 9f7bae44] joining:")
            && retry.log_line.contains("redemption not landed")
            && retry.log_line.contains("bouncer rejected"),
        "parked retry should keep the invite wording and source detail: {}",
        retry.log_line
    );
}

/// the boundary fetch retries forever, so its warn is paced: the first
/// failure speaks at once (a sync that never starts says why), then every
/// Nth — for a joiner and a resident alike.
#[test]
fn a_failing_boundary_fetch_warns_first_then_every_nth() {
    let every = crate::constants::BOUNDARY_FETCH_WARN_EVERY;
    for resident_standing in [false, true] {
        let speaks =
            |failures| joiner_manifest_fetch_retry("n", resident_standing, failures, "e").is_some();
        assert!(speaks(1), "the first failure must be visible immediately");
        for failures in 2..every {
            assert!(!speaks(failures), "failure {failures} stays silent");
        }
        assert!(speaks(every));
        assert!(!speaks(every + 1));
        assert!(speaks(2 * every));
    }
}

async fn served_directory_frame(
    expected: &mut Host,
    signer: &ed25519::PrivateKey,
    height: u64,
    seq: u64,
    msg: Msg,
) -> statesync::FinalizedFrame {
    let frame = node::encode_frame(signer, seq, &msg);
    let (origin, msg) = node::decode_frame(&frame).expect("decode frame");
    // a block is a BATCH super-frame: apply the single member via the batch
    // API and serve the batch bytes, so the catch-up replay reproduces this.
    expected
        .submit_block(
            host::BlockContext {
                height,
                consensus_time: height,
                origin: origin.clone(),
            },
            vec![(origin, msg)],
        )
        .await
        .expect("apply");
    statesync::FinalizedFrame {
        height,
        frame: node::encode_batch(&[frame]),
        disposition: statesync::FrameDisposition::Applied,
        roots: expected.module_roots(),
        root_hash: expected.root_hash(),
    }
}

async fn served_mixed_frame(
    expected: &mut Host,
    signer: &ed25519::PrivateKey,
    height: u64,
    seq: u64,
    value: u8,
) -> statesync::FinalizedFrame {
    let frame = node::encode_frame(
        signer,
        seq,
        &Msg {
            target: "disk".into(),
            payload: vec![value],
        },
    );
    let (origin, msg) = node::decode_frame(&frame).expect("decode frame");
    // a block is a BATCH super-frame: apply the single member through the
    // batch API so the served root-hash matches what recovery reproduces on
    // replay (which decodes the frame as a batch), and serve the batch bytes.
    expected
        .submit_block(
            host::BlockContext {
                height,
                consensus_time: height,
                origin: origin.clone(),
            },
            vec![(origin, msg)],
        )
        .await
        .expect("apply mixed frame");
    statesync::FinalizedFrame {
        height,
        frame: node::encode_batch(&[frame]),
        disposition: statesync::FrameDisposition::Applied,
        roots: expected.module_roots(),
        root_hash: expected.root_hash(),
    }
}

#[test]
fn floor_cert_view_must_map_to_boundary_height() {
    assert!(assert_floor_binds_view(30, 36, 6).is_ok());
    assert!(assert_floor_binds_view(30, 36, 4).is_err());
}

#[test]
fn promotion_boundary_prefers_latest_same_state_height() {
    let host_hash = test_root(7);
    let latest = test_manifest(12, host_hash, Some(vec![2]));

    match choose_promotion_boundary(host_hash, &latest, &test_me()) {
        PromotionBoundary::Promote { boundary, source } => {
            assert_eq!(boundary.height, 12);
            assert_eq!(source, PromotionBoundarySource::Latest);
        }
        PromotionBoundary::Retry => panic!("same-state latest boundary should promote"),
    }
}

#[test]
fn promotion_boundary_retries_when_latest_excludes_self() {
    let host_hash = test_root(7);
    let me = vec![1u8; 32];
    let latest = test_manifest_with_participants(12, host_hash, Some(vec![2]), vec![vec![9u8; 32]]);

    match choose_promotion_boundary(host_hash, &latest, &me) {
        PromotionBoundary::Retry => {}
        PromotionBoundary::Promote { .. } => {
            panic!("latest boundary excluding this node must not promote")
        }
    }
}

#[test]
fn promotion_boundary_accepts_latest_at_view_base_without_floor() {
    let host_hash = test_root(7);
    let latest = test_manifest_with_base(12, 12, host_hash, None);

    match choose_promotion_boundary(host_hash, &latest, &test_me()) {
        PromotionBoundary::Promote { boundary, source } => {
            assert_eq!(boundary.height, 12);
            assert_eq!(source, PromotionBoundarySource::Latest);
        }
        PromotionBoundary::Retry => panic!("view-base latest boundary should promote"),
    }
}

#[test]
fn promotion_boundary_requires_latest_floor_past_view_base() {
    let host_hash = test_root(7);
    let latest = test_manifest_with_base(12, 10, host_hash, None);

    match choose_promotion_boundary(host_hash, &latest, &test_me()) {
        PromotionBoundary::Retry => {}
        PromotionBoundary::Promote { .. } => {
            panic!("past-base latest boundary without a floor should retry")
        }
    }
}

#[test]
fn promotion_boundary_retries_when_latest_changed() {
    let host_hash = test_root(7);
    let latest = test_manifest(12, test_root(9), Some(vec![2]));

    match choose_promotion_boundary(host_hash, &latest, &test_me()) {
        PromotionBoundary::Retry => {}
        PromotionBoundary::Promote { .. } => {
            panic!("changed latest boundary should retry")
        }
    }
}

#[test]
fn promotion_boundary_retries_when_no_manifest_matches_host() {
    let host_hash = test_root(7);
    let latest = test_manifest(12, test_root(9), Some(vec![2]));

    match choose_promotion_boundary(host_hash, &latest, &test_me()) {
        PromotionBoundary::Retry => {}
        PromotionBoundary::Promote { .. } => panic!("changed roots should retry"),
    }
}

#[test]
fn suffix_installer_rejects_mismatched_served_seal() {
    let executor = commonware_runtime::deterministic::Runner::default();
    executor.start(|_| async move {
        let signer = commonware_cryptography::ed25519::PrivateKey::from_seed(77);
        let msg = Msg {
            target: "directory".into(),
            payload: encode_msg(&DirMsg::Set {
                key: "k".into(),
                value: "v".into(),
            }),
        };
        let frame = node::encode_frame(&signer, 0, &msg);

        let mut expected_host =
            Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let (origin, msg) = node::decode_frame(&frame).expect("decode frame");
        expected_host
            .submit_at(
                host::BlockContext {
                    height: 1,
                    consensus_time: 1,
                    origin,
                },
                msg,
            )
            .await
            .expect("apply");

        let served = statesync::FinalizedFrame {
            height: 1,
            frame: node::encode_batch(&[frame]),
            disposition: statesync::FrameDisposition::Applied,
            roots: expected_host.module_roots(),
            root_hash: StateRoot([0xA5; sdk::ROOT_LEN]),
        };
        let mut host = Host::genesis(vec![Box::new(Directory::new("directory"))]).expect("genesis");
        let prepared = host.prepare_work(served.height).await.expect("prepare");
        let err = apply_verified_suffix_frame(
            &mut host,
            &served,
            prepared,
            &host::NoCodeSource,
            &mut host::NoWitness,
            &[],
        )
        .await
        .expect_err("served seal mismatch must abort");
        assert!(
            err.contains("served seal"),
            "unexpected mismatch error: {err}"
        );
    });
}

/// a batch served at a height its source REFUSED (`batch_replayed`): the
/// catching-up host already applied it inside the window it seated with, so it
/// lands Rejected with nothing applied — and only the window makes it so.
#[test]
fn suffix_installer_refuses_a_batch_its_boundary_window_holds() {
    commonware_runtime::deterministic::Runner::default().start(|_| async move {
        let signer = ed25519::PrivateKey::from_seed(84);
        let mut host = fresh_directory_host();
        let applied = served_directory_frame(&mut host, &signer, 1, 0, dir_set("a", "1")).await;
        let roots = host.module_roots();
        let root_hash = host.root_hash();
        let served = statesync::FinalizedFrame {
            height: 2,
            disposition: statesync::FrameDisposition::Rejected,
            ..applied.clone()
        };
        let window = [(1, node::frame_id(&applied.frame))];

        let prepared = host.prepare_work(served.height).await.unwrap();
        let dispatches = apply_verified_suffix_frame(
            &mut host,
            &served,
            prepared,
            &host::NoCodeSource,
            &mut host::NoWitness,
            &window,
        )
        .await
        .expect("a replayed batch lands Rejected, as its source sealed it");
        assert!(dispatches.is_empty());
        assert_eq!(host.module_roots(), roots);
        assert_eq!(host.root_hash(), root_hash);

        // the window is the whole verdict: without it the batch re-applies.
        let prepared = host.prepare_work(served.height).await.unwrap();
        let err = apply_verified_suffix_frame(
            &mut host,
            &served,
            prepared,
            &host::NoCodeSource,
            &mut host::NoWitness,
            &[],
        )
        .await
        .expect_err("outside the window the replayed batch re-applies");
        assert!(err.contains("served seal mismatch"), "{err}");
    });
}

/// the suffix's own frames enter the window as they seal, so a batch replayed
/// WITHIN one catch-up run is refused too — and the window the run ends with
/// is the one a restart restores off the same journal.
#[test]
fn suffix_catchup_refuses_a_batch_it_sealed_earlier_in_the_run() {
    commonware_runtime::deterministic::Runner::default().start(|context| async move {
        let signer = ed25519::PrivateKey::from_seed(85);
        let mut expected = fresh_directory_host();
        let applied = served_directory_frame(&mut expected, &signer, 1, 0, dir_set("a", "1")).await;
        let replayed = statesync::FinalizedFrame {
            height: 2,
            disposition: statesync::FrameDisposition::Rejected,
            ..applied.clone()
        };

        let mut host = fresh_directory_host();
        let base = Manifest::capture(&host, None, 0, 0, vec![test_me()], vec![], None, 0, 1)
            .expect("base manifest");
        let mut recovery = Recovery::open(context.child("catchup_replayed"))
            .await
            .expect("open recovery");
        recovery.write_manifest(&base).await.unwrap();
        let store = consensus::ContentStore::new();
        let mut window = Vec::new();
        let caught = apply_suffix_frames(
            &mut recovery,
            &mut host,
            0,
            2,
            vec![applied.clone(), replayed],
            None,
            &store,
            &mut window,
        )
        .await
        .expect("a replayed height must not stop catch-up");
        assert_eq!(caught.applied, 2);
        assert_eq!(host.root_hash(), expected.root_hash());
        let batch = node::frame_id(&applied.frame);
        assert_eq!(window, vec![(1, batch), (2, batch)]);
        let journaled = recovery
            .read_finalized_frames(0, 2, usize::MAX)
            .await
            .unwrap();
        assert_eq!(journaled[1].disposition, node::Disposition::Rejected);
        drop(host);
        drop(recovery);

        let mut reopened = Recovery::open(context.child("reopen_catchup_replayed"))
            .await
            .unwrap();
        let mut restarted = fresh_directory_host();
        let recovered = reopened.recover(&mut restarted, &base).await.unwrap();
        assert_eq!(recovered.height, Some(2));
        assert_eq!(recovered.root_hash, expected.root_hash());
        assert_eq!(recovered.applied_frames, window);
    });
}

#[test]
fn suffix_catchup_refuses_to_commit_without_its_execution_witness() {
    struct RefuseWitness;
    #[async_trait::async_trait(?Send)]
    impl host::CommitWitness for RefuseWitness {
        async fn record(&mut self, height: u64, witness: &host::Witness) -> Result<(), String> {
            assert_eq!(height, 1);
            assert!(!witness.units.is_empty());
            Err("witness storage unavailable".into())
        }
    }

    commonware_runtime::deterministic::Runner::default().start(|_| async move {
        let signer = ed25519::PrivateKey::from_seed(77);
        let frame = node::encode_frame(
            &signer,
            0,
            &Msg {
                target: "directory".into(),
                payload: encode_msg(&DirMsg::Set {
                    key: "k".into(),
                    value: "v".into(),
                }),
            },
        );
        let mut host = fresh_directory_host();
        let before = host.root_hash();
        let served = statesync::FinalizedFrame {
            height: 1,
            frame: node::encode_batch(&[frame]),
            disposition: statesync::FrameDisposition::Applied,
            roots: host.module_roots(),
            root_hash: before,
        };
        let prepared = host.prepare_work(1).await.unwrap();
        let error = apply_verified_suffix_frame(
            &mut host,
            &served,
            prepared,
            &host::NoCodeSource,
            &mut RefuseWitness,
            &[],
        )
        .await
        .expect_err("an unjournaled execution must not commit");
        assert!(error.contains("witness storage unavailable"), "{error}");
        assert_eq!(host.root_hash(), before);
    });
}

#[test]
fn suffix_catchup_applies_verifies_and_journals_served_frames() {
    let executor = commonware_runtime::deterministic::Runner::default();
    executor.start(|context| async move {
        let signer = ed25519::PrivateKey::from_seed(78);
        let mut expected = fresh_directory_host();
        let frames = vec![
            served_directory_frame(&mut expected, &signer, 1, 0, dir_set("a", "1")).await,
            served_directory_frame(&mut expected, &signer, 2, 1, dir_set("b", "2")).await,
        ];

        let mut host = fresh_directory_host();
        let mut recovery = Recovery::open(context.child("post_catchup_ok"))
            .await
            .expect("open recovery");
        let store = consensus::ContentStore::new();
        let applied = apply_suffix_frames(
            &mut recovery,
            &mut host,
            0,
            2,
            frames.clone(),
            None,
            &store,
            &mut Vec::new(),
        )
        .await
        .expect("catch up");

        assert_eq!(applied.applied, 2);
        assert_eq!(host.root_hash(), expected.root_hash());
        assert_eq!(dir_value(&host, "a").await.as_deref(), Some("1"));
        assert_eq!(dir_value(&host, "b").await.as_deref(), Some("2"));
        let journaled = recovery
            .read_finalized_frames(0, 2, usize::MAX)
            .await
            .expect("read frames");
        assert_eq!(journaled.len(), 2);
        assert_eq!(journaled[0].height, 1);
        assert_eq!(journaled[1].height, 2);
    });
}

#[test]
fn suffix_catchup_recovers_a_matching_unsealed_execution() {
    commonware_runtime::deterministic::Runner::default().start(|context| async move {
        let signer = ed25519::PrivateKey::from_seed(83);
        let mut expected_host = fresh_directory_host();
        let served =
            served_directory_frame(&mut expected_host, &signer, 1, 0, dir_set("a", "1")).await;
        let mut host = fresh_directory_host();
        let base =
            Manifest::capture(&host, None, 0, 0, vec![test_me()], vec![], None, 0, 1).unwrap();
        let mut recovery = Recovery::open(context.child("before_suffix_seal"))
            .await
            .unwrap();
        recovery.write_manifest(&base).await.unwrap();
        let prepared = host.prepare_work(served.height).await.unwrap();
        recovery
            .pre_apply_catchup(
                &served.frame,
                &prepared,
                &node::BlockSeal {
                    height: served.height,
                    disposition: node::Disposition::Applied,
                    roots: served.roots.clone(),
                    root_hash: served.root_hash,
                },
            )
            .await
            .unwrap();
        apply_verified_suffix_frame(
            &mut host,
            &served,
            prepared,
            &host::NoCodeSource,
            &mut recovery,
            &[],
        )
        .await
        .unwrap();
        drop(host);
        drop(recovery);

        let mut recovery = Recovery::open(context.child("after_suffix_crash"))
            .await
            .unwrap();
        let mut host = fresh_directory_host();
        let recovered = recovery.recover(&mut host, &base).await.unwrap();
        assert_eq!(recovered.height, Some(1));
        assert_eq!(recovered.root_hash, served.root_hash);
        assert_eq!(dir_value(&host, "a").await.as_deref(), Some("1"));
        let frames = recovery
            .read_finalized_frames(0, 1, usize::MAX)
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].root_hash, served.root_hash);
    });
}

#[test]
fn suffix_catchup_reconciles_mixed_durability_state() {
    let executor = commonware_runtime::deterministic::Runner::default();
    executor.start(|context| async move {
        let signer = ed25519::PrivateKey::from_seed(81);
        let durable_store = TestDiskStore::default();
        let base_host = mixed_durability_host(durable_store.clone(), 0);
        let base_manifest =
            Manifest::capture(&base_host, None, 0, 0, vec![test_me()], vec![], None, 0, 1)
                .expect("base manifest");

        let mut expected = mixed_durability_host(TestDiskStore::default(), 0);
        let served = served_mixed_frame(&mut expected, &signer, 1, 0, 7).await;
        let target = statesync::Manifest {
            height: 1,
            root_hash: served.root_hash,
            epoch: 0,
            view_base: 0,
            participants: vec![test_me()],
            residents: vec![],
            floor_cert: Some(vec![1, 2, 3]),
            entries: vec![],
            applied_frames: vec![],
            pending_cutover_view: None,
        };

        let mut host = mixed_durability_host(durable_store.clone(), 0);
        let mut recovery = Recovery::open(context.child("post_catchup_mixed"))
            .await
            .expect("open recovery");
        recovery
            .write_manifest(&base_manifest)
            .await
            .expect("write base manifest");
        let store = consensus::ContentStore::new();
        let applied = apply_suffix_frames(
            &mut recovery,
            &mut host,
            0,
            1,
            vec![served],
            None,
            &store,
            &mut Vec::new(),
        )
        .await
        .expect("catch up");

        assert_eq!(applied.applied, 1);
        assert_eq!(
            durable_store.get(),
            7,
            "disk cohort committed the catch-up block durably"
        );

        // an old-base replay reconciles the torn sealed block via selective
        // replay (the still-at-pre memory cohort recommits, the already-
        // durable disk cohort aborts) rather than fail-stopping; the
        // checkpoint's value below is recovering WITHOUT that replay.
        let mut torn_host = restore_mixed_durability_host(durable_store.clone(), &base_manifest);
        let healed = recovery
            .recover(&mut torn_host, &base_manifest)
            .await
            .expect("old base replay heals the torn sealed block selectively");
        assert_eq!(healed.height, Some(1));
        assert_eq!(healed.root_hash, target.root_hash);
        assert_eq!(healed.applied, 1, "the torn suffix frame was replayed");
        assert_eq!(
            durable_store.get(),
            7,
            "disk cohort stays at its durable post-state"
        );
    });
}

#[test]
fn suffix_catchup_aborts_on_mismatched_served_seal() {
    enum Mismatch {
        Disposition,
        Roots,
        Hash,
    }
    for mismatch in [Mismatch::Disposition, Mismatch::Roots, Mismatch::Hash] {
        commonware_runtime::deterministic::Runner::default().start(|context| async move {
            let signer = ed25519::PrivateKey::from_seed(79);
            let mut expected = fresh_directory_host();
            let mut served =
                served_directory_frame(&mut expected, &signer, 1, 0, dir_set("a", "1")).await;
            match mismatch {
                Mismatch::Disposition => served.disposition = statesync::FrameDisposition::Rejected,
                Mismatch::Roots => served.roots[0].1 = test_root(0xA5),
                Mismatch::Hash => served.root_hash = test_root(0xA5),
            }

            let mut host = fresh_directory_host();
            let base = Manifest::capture(&host, None, 0, 0, vec![test_me()], vec![], None, 0, 1)
                .expect("base manifest");
            let mut recovery = Recovery::open(context.child("post_catchup_mismatch"))
                .await
                .expect("open recovery");
            recovery.write_manifest(&base).await.unwrap();
            let store = consensus::ContentStore::new();
            let err = apply_suffix_frames(
                &mut recovery,
                &mut host,
                0,
                1,
                vec![served],
                None,
                &store,
                &mut Vec::new(),
            )
            .await
            .expect_err("seal mismatch must abort");
            assert!(err.contains("served seal"), "{err}");
            assert_eq!(dir_value(&host, "a").await.as_deref(), Some("1"));
            drop(host);
            drop(recovery);

            let mut reopened = Recovery::open(context.child("reopen_mismatched_catchup"))
                .await
                .unwrap();
            let mut restarted = fresh_directory_host();
            let error = reopened.recover(&mut restarted, &base).await.unwrap_err();
            assert!(matches!(error, recovery::Error::Verify(_)), "{error}");
            assert!(error.to_string().contains("served seal"), "{error}");
            assert!(
                reopened
                    .read_finalized_frames(0, 1, usize::MAX)
                    .await
                    .is_err(),
                "recovery must not publish the failed suffix as finalized history"
            );
        });
    }
}

#[test]
fn suffix_catchup_is_noop_when_there_is_no_gap() {
    let executor = commonware_runtime::deterministic::Runner::default();
    executor.start(|context| async move {
        let mut host = fresh_directory_host();
        let before = host.root_hash();
        let mut recovery = Recovery::open(context.child("post_catchup_noop"))
            .await
            .expect("open recovery");
        let store = consensus::ContentStore::new();
        let applied = apply_suffix_frames(
            &mut recovery,
            &mut host,
            5,
            5,
            Vec::new(),
            None,
            &store,
            &mut Vec::new(),
        )
        .await
        .expect("noop catch up");

        assert_eq!(applied.applied, 0);
        assert_eq!(host.root_hash(), before);
        assert!(
            recovery
                .read_finalized_frames(5, 5, usize::MAX)
                .await
                .expect("empty range")
                .is_empty()
        );
    });
}

#[test]
fn suffix_catchup_accepts_a_height_gap_from_a_discarded_view() {
    let executor = commonware_runtime::deterministic::Runner::default();
    executor.start(|context| async move {
        let signer = ed25519::PrivateKey::from_seed(82);
        let mut expected = fresh_directory_host();
        // heights 1 and 3, skipping 2: the cutover ceiling discards a
        // straggler view on every honest node, so a real suffix can skip a
        // height without any block being omitted. strictly increasing, in
        // range, is the whole shape a healthy suffix must hold — see the
        // comment on `apply_suffix_frames`.
        let served_1 =
            served_directory_frame(&mut expected, &signer, 1, 0, dir_set("a", "1")).await;
        let served_3 =
            served_directory_frame(&mut expected, &signer, 3, 1, dir_set("b", "2")).await;

        let mut host = fresh_directory_host();
        let mut recovery = Recovery::open(context.child("post_catchup_gap"))
            .await
            .expect("open recovery");
        let store = consensus::ContentStore::new();
        let applied = apply_suffix_frames(
            &mut recovery,
            &mut host,
            0,
            3,
            vec![served_1, served_3],
            None,
            &store,
            &mut Vec::new(),
        )
        .await
        .expect("a skipped-view gap must not be refused");
        assert_eq!(applied.applied, 2);
        assert_eq!(host.root_hash(), expected.root_hash());
    });
}

/// a `SyncClient` that always answers the manifest request with a fixed,
/// canned manifest — enough to drive `catch_up_suffix_frames`'s trust gate
/// without a real server.
#[derive(Clone)]
struct FixedManifestClient {
    manifest: statesync::Manifest,
    rotations: Arc<std::sync::atomic::AtomicU32>,
}

impl statesync::SyncClient for FixedManifestClient {
    fn request(
        &self,
        _req: statesync::SyncRequest,
    ) -> impl std::future::Future<Output = Result<statesync::SyncResponse, statesync::SyncError>> + Send
    {
        let manifest = self.manifest.clone();
        async move { Ok(statesync::SyncResponse::Manifest(manifest)) }
    }
}

impl crate::blob_fetch::SourceRotate for FixedManifestClient {
    fn rotate_source(&self) {
        self.rotations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[test]
fn catch_up_suffix_frames_refuses_an_unanchored_tip_and_rotates_source() {
    let executor = commonware_runtime::deterministic::Runner::default();
    executor.start(|context| async move {
        let mut host = fresh_directory_host();
        let mut recovery = Recovery::open(context.child("catchup_unanchored_tip"))
            .await
            .expect("open recovery");
        let rotations = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let client = FixedManifestClient {
            // a served tip naming a participant set this node never trusted —
            // the "3 keys plus the victim's" shape #1815 describes.
            manifest: test_manifest_with_participants(1, test_root(1), None, vec![vec![9u8; 32]]),
            rotations: rotations.clone(),
        };
        let founding_participants = vec![test_me()];
        let anchor = crate::sync::serve::TrustAnchor {
            epoch: 0,
            participants: &founding_participants,
        };
        let store = consensus::ContentStore::new();
        let err = crate::sync::catchup::catch_up_suffix_frames(
            &client,
            &mut recovery,
            &mut host,
            None,
            0,
            1,
            b"ns",
            anchor,
            &store,
            Vec::new(),
        )
        .await
        .expect_err("a tip naming an unanchored participant set must be refused");
        assert!(
            matches!(err, crate::sync::catchup::SuffixCatchupError::Retry(_)),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            rotations.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "an unanchored tip must rotate away from the source that served it"
        );
    });
}

/// serves `manifests` in order, one per request, repeating the last.
#[derive(Clone)]
struct SequencedManifestClient {
    manifests: Vec<statesync::Manifest>,
    served: Arc<std::sync::atomic::AtomicUsize>,
    rotations: Arc<std::sync::atomic::AtomicU32>,
}

impl statesync::SyncClient for SequencedManifestClient {
    fn request(
        &self,
        _req: statesync::SyncRequest,
    ) -> impl std::future::Future<Output = Result<statesync::SyncResponse, statesync::SyncError>> + Send
    {
        let next = self
            .served
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .min(self.manifests.len() - 1);
        let manifest = self.manifests[next].clone();
        async move { Ok(statesync::SyncResponse::Manifest(manifest)) }
    }
}

impl crate::blob_fetch::SourceRotate for SequencedManifestClient {
    fn rotate_source(&self) {
        self.rotations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[test]
fn sync_only_boot_adopts_only_a_manifest_tied_to_its_founding_anchor() {
    let executor = commonware_runtime::deterministic::Runner::default();
    executor.start(|context| async move {
        // served bare, as a boundary at its epoch base is.
        let anchored = test_manifest_with_base(2, 2, test_root(2), None);
        let client = SequencedManifestClient {
            manifests: vec![
                // a boundary naming a participant set this node never trusted.
                test_manifest_with_participants(1, test_root(1), None, vec![vec![9u8; 32]]),
                anchored.clone(),
            ],
            served: Default::default(),
            rotations: Default::default(),
        };
        let founding_participants = vec![test_me()];
        let anchor = crate::sync::serve::TrustAnchor {
            epoch: 0,
            participants: &founding_participants,
        };
        let metrics = noded::NodeMetrics::register(&context);
        let adopted = crate::boot::sync_only::fetch_anchored_manifest(
            &context, &client, b"ns", anchor, &metrics, "test",
        )
        .await;
        assert_eq!(
            adopted.root_hash, anchored.root_hash,
            "the sync-only boot must skip the unanchored boundary and adopt the anchored one"
        );
        assert_eq!(
            client.rotations.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "an unanchored boundary must rotate away from the source that served it"
        );
    });
}

/// a source that serves an anchored tip but has pruned the frames below it.
#[derive(Clone)]
struct PrunedSourceClient {
    manifest: statesync::Manifest,
    rotations: Arc<std::sync::atomic::AtomicU32>,
}

impl statesync::SyncClient for PrunedSourceClient {
    fn request(
        &self,
        req: statesync::SyncRequest,
    ) -> impl std::future::Future<Output = Result<statesync::SyncResponse, statesync::SyncError>> + Send
    {
        let reply = match req {
            statesync::SyncRequest::Manifest => {
                statesync::SyncResponse::Manifest(self.manifest.clone())
            }
            _ => statesync::SyncResponse::RangePruned {
                requested_after: 0,
                retained_from: self.manifest.height,
            },
        };
        async move { Ok(reply) }
    }
}

impl crate::blob_fetch::SourceRotate for PrunedSourceClient {
    fn rotate_source(&self) {
        self.rotations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[test]
fn catch_up_suffix_frames_rotates_away_from_a_source_that_pruned_past_us() {
    let executor = commonware_runtime::deterministic::Runner::default();
    executor.start(|context| async move {
        let mut host = fresh_directory_host();
        let mut recovery = Recovery::open(context.child("catchup_range_pruned"))
            .await
            .expect("open recovery");
        let rotations = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let client = PrunedSourceClient {
            // anchored (the founding set) and bare at its own epoch base, so
            // the tip passes the trust gate and the frame fetch is reached.
            manifest: test_manifest_with_base(5, 5, test_root(5), None),
            rotations: rotations.clone(),
        };
        let founding_participants = vec![test_me()];
        let anchor = crate::sync::serve::TrustAnchor {
            epoch: 0,
            participants: &founding_participants,
        };
        let store = consensus::ContentStore::new();
        let err = crate::sync::catchup::catch_up_suffix_frames(
            &client,
            &mut recovery,
            &mut host,
            None,
            0,
            1,
            b"ns",
            anchor,
            &store,
            Vec::new(),
        )
        .await
        .expect_err("a source that pruned past us cannot serve the suffix");
        assert!(
            matches!(err, crate::sync::catchup::SuffixCatchupError::Retry(_)),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            rotations.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the retry must ask another source: this one can only prune further"
        );
    });
}

// ---- explorer-row rebuild (boot fold == live drain) ---------------------

fn row_dispatches(payload: &[u8], origin: &sdk::Origin) -> Vec<host::DispatchRecord> {
    vec![host::DispatchRecord {
        module: "directory".into(),
        origin: origin.clone(),
        cause: sdk::Cause::Direct,
        payload: payload.to_vec(),
        emitted_msgs: 0,
        emitted_events: 0,
        output: None,
        assigned: Vec::new(),
    }]
}

/// the boot fold rebuilds a block's per-op rows from its sealed BATCH frame,
/// re-staging each op's payload so `GET /v1/files/blob/{op_hash}` answers
/// again after a restart. the block coordinates and every op's identity
/// (proposer/target/payload/op_hash) match the drain's live row; the only
/// difference is the per-op dispatch TRACE — recovery folds the block-level
/// aggregate, not per-member, so a replayed op carries an empty trace (a
/// documented degradation visible only when the index is rebuilt).
#[test]
fn boot_fold_rebuilds_a_batch_block_ops() {
    let signer = ed25519::PrivateKey::from_seed(42);
    let payload = br#"{"set":{"key":"who","value":"ducktape"}}"#.to_vec();
    let msg = Msg {
        target: "directory".into(),
        payload: payload.clone(),
    };
    let frame = node::encode_frame(&signer, 1, &msg);
    let (origin, decoded) = node::decode_frame(&frame).expect("frame decodes");
    let dispatches = row_dispatches(&payload, &origin);
    let root_hash = test_root(9);

    // the drain's construction: one member op with its full dispatch trace.
    let drain_blobs = blobstore::BlobHandle::default();
    let drain_row = noded::block_row(&noded::BlockRecord {
        height: 7,
        hash: noded::hex_bytes(&node::frame_id(&frame)),
        commit_hash: hex(&root_hash),
        ops: vec![project_root_op(
            &drain_blobs,
            &origin,
            &decoded.target,
            &decoded.payload,
            &dispatches,
            noded::BlockDisposition::Applied,
        )],
    });
    let drain: serde_json::Value = serde_json::from_slice(&drain_row).unwrap();

    // the boot fold's construction: the sealed frame is a BATCH.
    let batch = node::encode_batch(std::slice::from_ref(&frame));
    let fold_blobs = blobstore::BlobHandle::default();
    let fold_row = sealed_frame_block_row(
        &fold_blobs,
        &recovery::FoldedBlock {
            host: &host::Host::new(),
            height: 7,
            frame: &batch,
            disposition: node::Disposition::Applied,
            root_hash,
            dispatches: &dispatches,
            roots: &[],
        },
    )
    .expect("an applied non-nop batch rebuilds its row");
    let row: serde_json::Value = serde_json::from_slice(&fold_row).expect("row json");

    // block coordinates match the drain.
    assert_eq!(row["height"], 7);
    assert_eq!(row["hash"], noded::hex_bytes(&node::frame_id(&batch)));
    assert_eq!(row["commit_hash"], hex(&root_hash));
    assert_eq!(row["ops"].as_array().unwrap().len(), 1);
    // the op's identity matches the drain byte-for-byte.
    assert_eq!(row["ops"][0]["proposer"], drain["ops"][0]["proposer"]);
    assert_eq!(row["ops"][0]["target"], "directory");
    assert_eq!(row["ops"][0]["payload"], drain["ops"][0]["payload"]);
    assert_eq!(row["ops"][0]["op_hash"], drain["ops"][0]["op_hash"]);
    // the fold carries an empty per-op trace (recovery folds the aggregate).
    assert_eq!(row["ops"][0]["operations"].as_array().unwrap().len(), 0);

    // the rebuild re-staged the payload: op_hash is dereferencable again
    // from the FOLD's (fresh, post-restart) blob store.
    let op_digest = drain_blobs.put_chunk(payload.clone());
    assert_eq!(row["ops"][0]["op_hash"], noded::hex_bytes(&op_digest));
    assert!(fold_blobs.has_chunk(&op_digest));
}

/// the fold's `None` gates mirror the drain's: a heartbeat nop and an
/// undecodable frame produce no explorer row (the drain's `op` is `None` /
/// nop-filtered for exactly these).
#[test]
fn boot_fold_skips_nop_and_undecodable_frames() {
    let blobs = blobstore::BlobHandle::default();
    let signer = ed25519::PrivateKey::from_seed(43);
    let nop = node::encode_frame(
        &signer,
        1,
        &Msg {
            target: crate::constants::NOP_TARGET.into(),
            payload: Vec::new(),
        },
    );
    for frame in [nop.as_slice(), b"not a frame".as_slice()] {
        assert!(
            sealed_frame_block_row(
                &blobs,
                &recovery::FoldedBlock {
                    host: &host::Host::new(),
                    height: 3,
                    frame,
                    disposition: node::Disposition::Applied,
                    root_hash: test_root(1),
                    dispatches: &[],
                    roots: &[],
                },
            )
            .is_none()
        );
    }
}

/// a REJECTED sealed frame still gets its row (the drain writes one for a
/// decoded non-nop reject), with an empty dispatch trace.
#[test]
fn boot_fold_rebuilds_rejected_rows_with_empty_trace() {
    let blobs = blobstore::BlobHandle::default();
    let signer = ed25519::PrivateKey::from_seed(44);
    let frame = node::encode_frame(
        &signer,
        2,
        &Msg {
            target: "directory".into(),
            payload: b"garbage-the-module-rejects".to_vec(),
        },
    );
    let batch = node::encode_batch(&[frame]);
    let row = sealed_frame_block_row(
        &blobs,
        &recovery::FoldedBlock {
            host: &host::Host::new(),
            height: 5,
            frame: &batch,
            disposition: node::Disposition::Rejected,
            root_hash: test_root(2),
            dispatches: &[],
            roots: &[],
        },
    )
    .expect("a decoded non-nop reject still shows in the explorer");
    let row: serde_json::Value = serde_json::from_slice(&row).expect("row json");
    assert_eq!(row["ops"][0]["disposition"], "rejected");
    assert_eq!(
        row["ops"][0]["operations"].as_array().map(Vec::len),
        Some(0)
    );
}
