// Residents, joins by state sync, snapshots, a shrinking set and the contract roll.

use super::*;

/// One validator, and a resident the valset adds whose node never runs.
/// Only the validator is seated, so the chain finalizes on through the
/// epoch that records the resident; seating the resident too would ask a
/// quorum of two and stop it at that epoch's first block.
#[test]
fn an_offline_resident_leaves_the_chain_finalizing() {
    let root = tempfile::tempdir().unwrap();
    let seat = Seat::new(root.path(), "n0");
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let live = seat.start();
    let client = live.client.clone();
    let offline = ed25519::PrivateKey::from_seed(12);
    let (_unheard, address) = unheard();
    let unanswered = unanswered();
    let enroll = Op::Enroll {
        address: Some(address),
        halo: unanswered.local_addr().unwrap().to_string(),
        invite: None,
    };

    client_runtime().block_on(async {
        let recorded = membership(&client, &offline, enroll).await;
        let resident_epoch = recorded / EPOCH_LENGTH + 1;
        let beyond = resident_epoch * EPOCH_LENGTH + 1;
        // the chain finalizes past the first block of the resident's epoch
        reach(&client, beyond).await;
        seat.admin(&client, Admin::Shutdown).await;
    });
    live.thread.join().unwrap();
}

/// Two validators, and a stranger that joins: it enrolls, waits for the
/// epoch that seats it as a resident, and state-syncs inside it. The set of
/// two needs both, so once the second shuts down the chain stops inside that
/// epoch, and the resident follows at the tip however long it takes: it
/// comes within two blocks of the validator before the validator applies
/// the block that closes the epoch, so no next-epoch traffic carried it.
/// With the second validator back, the resident applies a change the
/// validators make after it started, and hears their finalize votes. The
/// second validator registers no address and dials out, so it comes back on
/// listeners it binds afresh.
#[test]
fn a_resident_follows_at_the_tip() {
    // long enough that the resident joins and the chain stops inside one
    // epoch
    const EPOCH: u64 = 100;
    let root = tempfile::tempdir().unwrap();
    let mut seats: Vec<Seat> = (0..2)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    seats[1].address = None;
    let mut resident = Seat::new(root.path(), "n2");
    resident.address = None;
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH);
    for seat in &seats {
        seat.init(&founding);
    }
    let mut live: Vec<Live> = seats.iter().map(Seat::start).collect();
    let client = live[0].client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    resident.join(&client);
    let epoch = runtime.block_on(client.status()).unwrap().epoch;
    let closing = (epoch + 1) * EPOCH - 1;
    runtime.block_on(seats[1].admin(&live[1].client, Admin::Shutdown));
    let stopped = live.pop().unwrap();
    stopped.thread.join().unwrap();
    let follower = resident.start();
    runtime.block_on(async {
        let mut blocks = follower.client.changes(host::RECENT).await.unwrap();
        loop {
            // the resident first, so the validator's height bounds it
            let followed = follower.client.status().await.unwrap().height;
            let validator = client.status().await.unwrap().height;
            assert!(
                validator < closing,
                "the resident was at {followed} once the validator applied {validator}: \
                 block {closing} closes epoch {epoch}, so the chain went on past the epoch \
                 the second validator left in"
            );
            if followed + 2 >= validator {
                return;
            }
            blocks.next().await.unwrap().unwrap();
        }
    });

    live.push(seats[1].start());
    runtime.block_on(async {
        let mut changes = follower.client.changes("probe").await.unwrap();
        let write = set(b"a", b"after");
        client
            .submit(frame(&client, &alice, vec![write]).await, Preconfirm::Off)
            .await
            .unwrap();
        let change = changes.next().await.unwrap().unwrap();
        assert_eq!(
            change.writes,
            vec![(b"a".to_vec(), Some(b"after".to_vec()))]
        );
        let mut blocks = follower.client.changes(host::RECENT).await.unwrap();
        loop {
            let network = follower.client.network().await.unwrap();
            let signed = |key: Vec<u8>| {
                let row = network.members.iter().find(|row| row.key == key);
                row.and_then(|row| row.signed)
            };
            if let Some(height) = signed(seats[0].key()) {
                assert_eq!(network.members.len(), 3, "{network:?}");
                assert_eq!(signed(resident.key()), None, "{network:?}");
                assert!(height <= network.height, "{network:?}");
                break;
            }
            blocks.next().await.unwrap().unwrap();
        }
        for (seat, node) in seats.iter().zip(&live).chain([(&resident, &follower)]) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live.into_iter().chain([follower]) {
        node.thread.join().unwrap();
    }
}

#[cfg(target_os = "linux")]
fn clones(dir: &Path) -> bool {
    let source = dir.join("clone-probe");
    std::fs::write(&source, b"probe").unwrap();
    let copy = std::fs::File::create_new(dir.join("clone-probe-copy")).unwrap();
    let source = std::fs::File::open(&source).unwrap();
    rustix::fs::ioctl_ficlone(&copy, &source).is_ok()
}

#[cfg(target_os = "linux")]
#[test]
fn a_resident_snapshots_each_epoch_where_its_directory_clones_and_a_validator_never_tries() {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let seat = Seat::new(root.path(), "n0");
    let mut resident = Seat::new(root.path(), "n1");
    resident.address = None;
    let founding = founding(root.path(), &[&seat], EPOCH_LENGTH);
    seat.init(&founding);
    let cloning = clones(root.path());
    let into = root.path().join("downloaded");
    let live = seat.start();
    let client = live.client.clone();
    let runtime = client_runtime();
    resident.join(&client);
    let follower = resident.start();
    let (closing, downloaded) = runtime.block_on(async {
        let joined = follower.client.status().await.unwrap().height;
        let closing = (joined / EPOCH_LENGTH + 1) * EPOCH_LENGTH - 1;
        reach(&follower.client, closing + EPOCH_LENGTH + 1).await;
        let downloaded = match cloning {
            true => Some(fetched_snapshot(&follower.client, &into).await),
            false => None,
        };
        seat.admin(&client, Admin::Shutdown).await;
        resident.admin(&follower.client, Admin::Shutdown).await;
        (closing, downloaded)
    });
    live.thread.join().unwrap();
    follower.thread.join().unwrap();

    assert!(!seat.workspace.dir().join("snapshots").exists());
    let snapshots = resident.workspace.dir().join("snapshots");
    assert!(snapshots.exists(), "the resident never tried a snapshot");
    let kept: Vec<String> = std::fs::read_dir(&snapshots)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    if !cloning {
        println!(
            "{} does not clone: the resident tried a snapshot, cleared snapshots/ and followed \
             past the next epoch",
            root.path().display()
        );
        assert!(kept.is_empty(), "{kept:?}");
        return;
    }
    println!(
        "{} clones: the resident keeps its newest snapshot",
        root.path().display()
    );
    let [newest] = kept.as_slice() else {
        panic!("the resident keeps one whole snapshot: {kept:?}");
    };
    let height: u64 = newest.parse().unwrap();
    assert!(
        height >= closing && (height + 1).is_multiple_of(EPOCH_LENGTH),
        "{height} after {closing}"
    );
    let snapshot = snapshots.join(newest);
    let manifest: statesync::Manifest =
        abi::decode(&std::fs::read(snapshot.join("manifest")).unwrap()).unwrap();
    assert_eq!(manifest.tip.height, height);
    assert!(
        manifest.targets.contains_key("valset"),
        "{:?}",
        manifest.targets.keys()
    );
    opens_at_its_targets(&snapshot.join("runtime"), manifest);
    let downloaded = downloaded.unwrap();
    let height = downloaded.tip.height;
    assert!(
        height >= closing && (height + 1).is_multiple_of(EPOCH_LENGTH),
        "the download is at {height}, after {closing}"
    );
    opens_at_its_targets(&into.join("runtime"), downloaded);
}

#[cfg(target_os = "linux")]
async fn fetched_snapshot(client: &Client, into: &Path) -> statesync::Manifest {
    let mut blocks = client.changes(host::RECENT).await.unwrap();
    loop {
        match client.snapshot(into).await {
            Ok(manifest) => return manifest,
            Err(noded::Error::Refused(refusal)) if refusal.reason == reason::NOT_FOUND => {
                blocks.next().await.unwrap().unwrap();
            }
            Err(error) => panic!("the resident's snapshot does not download: {error}"),
        }
    }
}

#[cfg(target_os = "linux")]
fn opens_at_its_targets(runtime: &Path, manifest: statesync::Manifest) {
    let (opened, targets) = runner(runtime).start(|context| async move {
        let mut opened = BTreeMap::new();
        for program in manifest.targets.keys() {
            let context = context.child("snapshot").with_attribute("program", program);
            let name = state::commitment_name(NETWORK, program);
            let commitment = state::Commitment::open(context, &name, 0).await.unwrap();
            opened.insert(program.clone(), commitment.target().unwrap().unwrap());
        }
        (opened, manifest.targets)
    });
    assert_eq!(opened, targets);
}

/// Three validators, and the valset drops the third. Once the chain is in
/// the epoch that seats two, the third node is shut down: the set of three
/// needed all three, so the two finalize on only because the shrink took
/// effect.
#[test]
fn a_shrunk_set_finalizes_without_the_removed_member() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..3)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats {
        seat.init(&founding);
    }
    let live: Vec<Live> = seats.iter().map(Seat::start).collect();
    let remove = Op::Vote(Motion::Remove {
        key: seats[2].key(),
    });

    client_runtime().block_on(async {
        let client = &live[0].client;
        let recorded = membership(client, &seats[0].identity, remove).await;
        let shrunk = recorded / EPOCH_LENGTH + 1;
        // the chain enters the epoch that seats two
        reach(client, shrunk * EPOCH_LENGTH).await;
        seats[2].admin(&live[2].client, Admin::Shutdown).await;
        let beyond = (shrunk + 1) * EPOCH_LENGTH + 1;
        // the two finalize across the next boundary
        for node in &live[..2] {
            reach(&node.client, beyond).await;
        }
        for (seat, node) in seats.iter().zip(&live).take(2) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live {
        node.thread.join().unwrap();
    }
}

fn refused_start(seat: &Seat) -> String {
    let workspace = seat.workspace.clone();
    runner(&workspace.runtime_dir()).start(|context| async move {
        let cadence = workspace
            .descriptor()
            .unwrap()
            .cadence(consensus::Timeouts::default())
            .unwrap();
        let started = noded::run_on(
            context,
            &workspace,
            Logs::install("info").unwrap(),
            listeners(),
            HaloConfig::default(),
            LoadConfig::default(),
            cadence,
        )
        .await;
        match started {
            Ok(_) => panic!("the node started on state from before the roll"),
            Err(error) => error.to_string(),
        }
    })
}

fn rolled(seat: &Seat, height: u64) -> Result<noded::Rolled, String> {
    let workspace = seat.workspace.clone();
    runner(&workspace.runtime_dir()).start(|context| async move {
        noded::roll(context, &workspace, height)
            .await
            .map_err(|error| error.to_string())
    })
}

#[test]
fn a_node_stopped_before_the_roll_runs_again_only_once_roll_syncs_it_to_a_head_past_it() {
    let root = tempfile::tempdir().unwrap();
    let validator = Seat::new(root.path(), "n0");
    let mut resident = Seat::new(root.path(), "n1");
    resident.address = None;
    let founding = founding(root.path(), &[&validator], EPOCH_LENGTH);
    validator.init(&founding);
    let live = validator.start();
    let client = live.client.clone();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    resident.join(&client);
    assert_eq!(
        resident.workspace.stamp().unwrap().contract,
        noded::NODE_CONTRACT
    );
    let follower = resident.start();
    runtime.block_on(resident.admin(&follower.client, Admin::Shutdown));
    follower.thread.join().unwrap();
    std::fs::remove_file(resident.workspace.dir().join("contract")).unwrap();
    assert!(refused_start(&resident).contains("applied under contract 9"));

    let written = runtime.block_on(async {
        let mut changes = client.changes("probe").await.unwrap();
        client
            .submit(
                frame(&client, &alice, vec![set(b"a", b"while it was down")]).await,
                Preconfirm::Off,
            )
            .await
            .unwrap();
        changes.next().await.unwrap().unwrap()
    });
    let roll = runtime.block_on(client.status()).unwrap().height;
    assert!(roll >= written.height);

    let synced = rolled(&resident, roll).unwrap();
    let noded::Rolled::Synced { stamp, aside } = synced else {
        panic!("a node behind the roll syncs: {synced:?}");
    };
    assert_eq!(stamp.contract, noded::NODE_CONTRACT);
    assert!(stamp.from > roll, "{stamp:?}");
    assert!(aside.join("state").exists());
    assert_eq!(rolled(&resident, roll), Ok(noded::Rolled::Already(stamp)));

    let follower = resident.start();
    runtime.block_on(async {
        let status = follower.client.status().await.unwrap();
        assert!(status.height + 1 >= stamp.from, "{status:?}");
        assert_eq!(
            follower
                .client
                .get(Layer::Confirmed, "probe", b"a")
                .await
                .unwrap(),
            Some(b"while it was down".to_vec())
        );
    });
    shut_down(&runtime, &[&validator, &resident], vec![live, follower]);

    let tip = runner(&validator.workspace.runtime_dir()).start(|context| {
        let workspace = validator.workspace.clone();
        async move {
            let descriptor = workspace.descriptor().unwrap();
            node::Node::open(context, &descriptor.network, workspace.dir())
                .await
                .unwrap()
                .tip()
                .unwrap()
                .height
        }
    });
    validator
        .workspace
        .write_stamp(&noded::Stamp {
            contract: 9,
            from: 0,
        })
        .unwrap();
    assert!(refused_start(&validator).contains("applied under contract 9"));
    assert!(
        rolled(&validator, tip - 1)
            .unwrap_err()
            .contains("past the roll height")
    );
    assert!(
        rolled(&validator, tip + 1)
            .unwrap_err()
            .contains("names no source")
    );
    let stamp = noded::Stamp {
        contract: noded::NODE_CONTRACT,
        from: tip + 1,
    };
    assert_eq!(rolled(&validator, tip), Ok(noded::Rolled::Stamped(stamp)));
    let live = validator.start();
    runtime.block_on(async {
        let mut blocks = live.client.changes(host::RECENT).await.unwrap();
        blocks.next().await.unwrap().unwrap();
    });
    shut_down(&runtime, &[&validator], vec![live]);
}

#[test]
fn a_late_validator_joins_by_state_sync_and_follows() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..4)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats[..3] {
        seat.init(&founding);
    }
    let live: Vec<Live> = seats[..3].iter().map(Seat::start).collect();
    let alice = ed25519::PrivateKey::from_seed(11);
    let runtime = client_runtime();

    let first = runtime.block_on(async {
        let mut changes = live[0].client.changes("probe").await.unwrap();
        live[0]
            .client
            .submit(
                frame(&live[0].client, &alice, vec![set(b"a", b"1")]).await,
                Preconfirm::Off,
            )
            .await
            .unwrap();
        changes.next().await.unwrap().unwrap()
    });
    assert!(first.height >= 1);

    seats[3].join(&live[0].client);
    let joined = seats[3].start();
    let synced = runtime.block_on(async {
        let status = joined.client.status().await.unwrap();
        assert!(status.height >= first.height, "{status:?}");
        assert_eq!(
            joined
                .client
                .get(Layer::Confirmed, "probe", b"a")
                .await
                .unwrap(),
            Some(b"1".to_vec())
        );
        let mut changes = joined.client.changes("probe").await.unwrap();
        live[1]
            .client
            .submit(
                frame(&live[1].client, &alice, vec![set(b"b", b"2")]).await,
                Preconfirm::Off,
            )
            .await
            .unwrap();
        changes.next().await.unwrap().unwrap()
    });
    assert_eq!(synced.writes, vec![(b"b".to_vec(), Some(b"2".to_vec()))]);

    runtime.block_on(async {
        for (seat, node) in seats
            .iter()
            .zip(live.iter().chain(std::iter::once(&joined)))
        {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in live.into_iter().chain(std::iter::once(joined)) {
        node.thread.join().unwrap();
    }
}

async fn spent_in(
    changes: &mut (impl futures::Stream<Item = noded::Result<noded::wire::Change>> + Unpin),
    id: &[u8],
) -> u64 {
    loop {
        let change = changes.next().await.unwrap().unwrap();
        let rows = change.writes.iter().filter_map(|(_, row)| row.as_deref());
        let mut ids = rows.flat_map(|row| row.chunks(id.len()));
        if ids.any(|carried| carried == id) {
            return change.height;
        }
    }
}

#[test]
fn a_resident_hands_every_frame_a_client_gives_it_to_the_validators() {
    let root = tempfile::tempdir().unwrap();
    let seats: Vec<Seat> = (0..3)
        .map(|i| Seat::new(root.path(), &format!("n{i}")))
        .collect();
    let resident = Seat::new(root.path(), "n3");
    let founding = founding(root.path(), &seats.iter().collect::<Vec<_>>(), EPOCH_LENGTH);
    for seat in &seats {
        seat.init(&founding);
    }
    let founders: Vec<Live> = seats.iter().map(Seat::start).collect();
    let bob = ed25519::PrivateKey::from_seed(12);
    let runtime = client_runtime();

    resident.join(&founders[0].client);
    let follower = resident.start();
    let given = |steps: Vec<Step>| runtime.block_on(frame(&follower.client, &bob, steps));

    let mut changes = runtime
        .block_on(founders[1].client.changes("probe"))
        .unwrap();
    let early = given(vec![set(b"early", b"at start")]);
    let early = runtime.block_on(preconfirmed(&follower.client, early));
    assert!(
        matches!(early.outcome, Outcome::Applied { .. }),
        "{early:?}"
    );
    // a frame given the instant the resident starts lands
    runtime.block_on(landed(&mut changes, &[b"early"]));

    let failing = given(vec![Step::Fail("no".into())]);
    let mut spent = runtime
        .block_on(founders[1].client.changes(host::SPENT))
        .unwrap();
    let predicted = runtime.block_on(preconfirmed(&follower.client, failing.clone()));
    assert!(
        matches!(predicted.outcome, Outcome::Rejected(_)),
        "{predicted:?}"
    );
    let carried = submission(&failing).id();
    // a frame predicted to fail is relayed and a block carries it
    runtime.block_on(spent_in(&mut spent, &carried));
    drop(spent);

    let nearly_a_block = node::BLOCK_BYTES - (1 << 16);
    let large: Vec<Vec<u8>> = (1..=2u8).map(|n| vec![n; nearly_a_block]).collect();
    for (n, value) in large.iter().enumerate() {
        let key = format!("large{n}");
        let large = given(vec![set(key.as_bytes(), value)]);
        runtime
            .block_on(follower.client.submit(large, Preconfirm::Off))
            .unwrap();
    }
    let heights = runtime.block_on(landed(&mut changes, &[b"large0", b"large1"]));
    let [one, two] = heights[..] else {
        panic!("both large frames land");
    };
    assert_ne!(one, two, "two nearly full frames share no block");
    drop(changes);

    let refused = runtime.block_on(
        follower
            .client
            .submit(vec![0; node::BLOCK_BYTES + 1], Preconfirm::Off),
    );
    let Err(noded::Error::Refused(refusal)) = refused else {
        panic!("a frame no block carries is refused: {refused:?}");
    };
    assert_eq!(refusal.reason, abi::reason::CAPACITY);

    runtime.block_on(async {
        for (seat, node) in seats.iter().zip(&founders).chain([(&resident, &follower)]) {
            seat.admin(&node.client, Admin::Shutdown).await;
        }
    });
    for node in founders.into_iter().chain([follower]) {
        node.thread.join().unwrap();
    }
}
