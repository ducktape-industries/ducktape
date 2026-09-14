//! The executor against the fake node: a published release is fetched and
//! verified, an archive is paged down and resumed, a wrong hash is refused,
//! and the wall tick drives the machine only while connected and quiet.
//! Every wait is on a request's reply; nothing here reads a clock.

use std::collections::BTreeMap;
use std::sync::Arc;

use app_update::{
    Artifact, Manifest, Phase, Platform, PublicKey, Refusal, Release, SCHEMA, Sha, Signature,
    layout,
};
use commonware_cryptography::{Signer as _, ed25519};

use super::*;
use crate::backend::view_source::tests::{FakeDeployment, fake_node};

fn release_key() -> ed25519::PrivateKey {
    ed25519::PrivateKey::from_seed(41)
}

fn keys() -> TrustedKeys {
    TrustedKeys {
        pinned: PublicKey::of(&release_key()),
        successor: None,
    }
}

fn archive() -> Vec<u8> {
    (0..(2 * 1024 * 1024 + 1024 * 512 + 9))
        .map(|i| (i % 239) as u8)
        .collect()
}

fn manifest(sequence: u64, archive: &[u8]) -> Manifest {
    Manifest {
        schema: SCHEMA,
        channel: layout::CHANNEL.into(),
        sequence,
        published_at: "2026-09-02T00:00:00Z".into(),
        release: Release {
            sha256_id: Sha::ZERO,
            display: "2026.09.2+abc1234".into(),
            node_contract: 1,
            notes_url: String::new(),
        },
        artifacts: BTreeMap::from([(
            Platform::HOST.key(),
            Artifact {
                sha256: Sha::digest(archive),
                size: archive.len() as u64,
            },
        )]),
        successor_key: None,
    }
    .sealed()
}

/// A fake node serving nothing yet; `publish` puts a signed release on it.
fn deployment() -> Arc<FakeDeployment> {
    let artifact = module_artifact::Artifact::Module(module_artifact::ModuleArtifact {
        component: vec![0],
        index: None,
        view: None,
    });
    FakeDeployment::serving("noop", &artifact)
}

fn publish(
    node: &FakeDeployment,
    manifest: &Manifest,
    signer: &ed25519::PrivateKey,
    archive: &[u8],
) {
    let bytes = serde_json::to_vec_pretty(manifest).unwrap();
    let signature = Signature::sign(signer, &bytes);
    node.publish_file(&layout::manifest_path(), bytes);
    node.publish_file(&layout::signature_path(), signature.encoded().into_bytes());
    let sha = manifest.artifacts[&Platform::HOST.key()].sha256;
    node.publish_file(
        &layout::archive_path(&sha, &Platform::HOST.key()),
        archive.to_vec(),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_verifies_a_published_release() {
    let node = deployment();
    let archive = archive();
    let manifest = manifest(3, &archive);
    publish(&node, &manifest, &release_key(), &archive);
    let client = fake_node(node.clone()).await;

    let verified = fetch(&client, &keys())
        .await
        .expect("served")
        .expect("verifies");
    assert_eq!(verified.manifest, manifest);
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_refuses_a_strangers_signature_and_none_when_unpublished() {
    let node = deployment();
    let archive = archive();
    let manifest = manifest(3, &archive);
    let stranger = ed25519::PrivateKey::from_seed(99);
    publish(&node, &manifest, &stranger, &archive);
    let client = fake_node(node.clone()).await;

    let refused = fetch(&client, &keys()).await.expect("served").unwrap_err();
    assert_eq!(refused, Refusal::BadSignature);

    node.withdraw_file(&layout::signature_path());
    assert!(
        fetch(&client, &keys()).await.is_none(),
        "no pair on the node is not a refusal"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn download_pages_the_archive_and_resumes_a_partial() {
    let node = deployment();
    let archive = archive();
    let manifest = manifest(3, &archive);
    publish(&node, &manifest, &release_key(), &archive);
    let client = fake_node(node.clone()).await;
    let sha = Sha::digest(&archive);
    let updates = tempfile::tempdir().unwrap();

    // a fresh download: three pages for 2.5 MiB.
    download(&client, updates.path(), sha, archive.len() as u64)
        .await
        .expect("complete");
    let landed = std::fs::read(partial_path(updates.path(), &sha)).unwrap();
    assert_eq!(landed, archive);
    let pages: Vec<u64> = node
        .file_reads
        .lock()
        .unwrap()
        .iter()
        .map(|(_, offset, _)| *offset)
        .collect();
    assert_eq!(pages, vec![0, 1024 * 1024, 2 * 1024 * 1024]);

    // a partial holding the first 1.5 MiB resumes from there.
    node.file_reads.lock().unwrap().clear();
    let cut = 1024 * 1024 + 512 * 1024;
    std::fs::write(partial_path(updates.path(), &sha), &archive[..cut]).unwrap();
    download(&client, updates.path(), sha, archive.len() as u64)
        .await
        .expect("resumed");
    let landed = std::fs::read(partial_path(updates.path(), &sha)).unwrap();
    assert_eq!(landed, archive);
    let pages: Vec<(u64, u64)> = node
        .file_reads
        .lock()
        .unwrap()
        .iter()
        .map(|(_, offset, len)| (*offset, *len))
        .collect();
    assert_eq!(
        pages,
        vec![(cut as u64, 1024 * 1024), (cut as u64 + 1024 * 1024, 9)],
        "only the missing tail was asked for"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn download_refuses_a_hash_mismatch_and_drops_the_partial() {
    let node = deployment();
    let archive = archive();
    let manifest = manifest(3, &archive);
    publish(&node, &manifest, &release_key(), &archive);
    let client = fake_node(node.clone()).await;
    let sha = Sha::digest(&archive);
    let updates = tempfile::tempdir().unwrap();

    // the node serves other bytes under the archive's path.
    let mut swapped = archive.clone();
    swapped[7] ^= 0xff;
    node.publish_file(&layout::archive_path(&sha, &Platform::HOST.key()), swapped);
    let reason = download(&client, updates.path(), sha, archive.len() as u64)
        .await
        .unwrap_err();
    assert_eq!(reason, "sha256_mismatch");
    assert!(
        !partial_path(updates.path(), &sha).exists(),
        "a mismatched partial is not kept for a resume"
    );

    // a partial longer than the archive is not this archive either.
    std::fs::create_dir_all(updates.path().join("releases")).unwrap();
    std::fs::write(
        partial_path(updates.path(), &sha),
        vec![0u8; archive.len() + 1],
    )
    .unwrap();
    let reason = download(&client, updates.path(), sha, archive.len() as u64)
        .await
        .unwrap_err();
    assert_eq!(reason, "partial_overlong");
    assert!(!partial_path(updates.path(), &sha).exists());
}

/// The wall tick runs the machine: nothing while disconnected, one fetch per
/// interval, no second job while one runs; a newer release is downloaded
/// and the phase persists as `Downloading`, complete on disk, with `Verify`
/// deferred to the next executor.
#[tokio::test(flavor = "current_thread")]
async fn tick_drives_fetch_then_download_while_connected() {
    let node = deployment();
    let archive = archive();
    let manifest = manifest(3, &archive);
    publish(&node, &manifest, &release_key(), &archive);
    let client = fake_node(node.clone()).await;
    let rpc = client.origin().to_string();
    let sha = Sha::digest(&archive);
    let updates = tempfile::tempdir().unwrap();
    let state_path = updates.path().join("state.json");
    let current = Sha::digest(b"running");
    let idle = Phase::Idle(app_update::Idle {
        current,
        previous: None,
        pinned_sequence: 0,
    });
    let mut updater = Updater::new(
        idle.clone(),
        keys(),
        updates.path().to_path_buf(),
        state_path.clone(),
    );

    assert_eq!(updater.tick(1_000, false), None, "not connected: no check");
    assert_eq!(updater.tick(1_000, true), Some(Job::Fetch));
    assert_eq!(updater.tick(1_001, true), None, "a fetch is in flight");

    let reply = run_job(
        rpc.clone(),
        keys(),
        updates.path().to_path_buf(),
        Job::Fetch,
    )
    .await;
    let next = updater.reply(reply);
    assert_eq!(
        next,
        Some(Job::Download {
            sha,
            size: archive.len() as u64
        })
    );
    assert!(
        matches!(updater.reading().phase, Phase::Downloading(_)),
        "the machine moved to Downloading"
    );
    let persisted =
        app_update::state::decode(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(
        persisted,
        updater.reading().phase,
        "Persist wrote the phase"
    );
    assert_eq!(
        updater.tick(1_000 + CHECK_INTERVAL_SECS, true),
        None,
        "a download is in flight: no fetch even when due"
    );

    let reply = run_job(rpc, keys(), updates.path().to_path_buf(), next.unwrap()).await;
    assert_eq!(reply, Some(Event::DownloadFinished { sha }));
    assert_eq!(updater.reply(reply), None, "Verify is deferred: no job");
    assert!(matches!(updater.reading().phase, Phase::Downloading(_)));
    assert_eq!(
        std::fs::read(partial_path(updates.path(), &sha)).unwrap(),
        archive
    );
}

/// The release that runs is the one the manifest names: `UpToDate`, and the
/// next check waits out the interval.
#[tokio::test(flavor = "current_thread")]
async fn tick_reports_up_to_date_and_waits_out_the_interval() {
    let node = deployment();
    let archive = archive();
    let manifest = manifest(3, &archive);
    publish(&node, &manifest, &release_key(), &archive);
    let client = fake_node(node.clone()).await;
    let rpc = client.origin().to_string();
    let updates = tempfile::tempdir().unwrap();
    let idle = Phase::Idle(app_update::Idle {
        current: Sha::digest(&archive),
        previous: None,
        pinned_sequence: 0,
    });
    let mut updater = Updater::new(
        idle,
        keys(),
        updates.path().to_path_buf(),
        updates.path().join("state.json"),
    );

    assert_eq!(updater.tick(5_000, true), Some(Job::Fetch));
    let reply = run_job(rpc, keys(), updates.path().to_path_buf(), Job::Fetch).await;
    assert_eq!(updater.reply(reply), None);
    assert_eq!(updater.reading().banner, Some(UpdateBanner::UpToDate));
    assert!(matches!(updater.reading().phase, Phase::Idle(_)));
    assert_eq!(updater.tick(5_000 + CHECK_INTERVAL_SECS - 1, true), None);
    assert_eq!(
        updater.tick(5_000 + CHECK_INTERVAL_SECS, true),
        Some(Job::Fetch)
    );
}

/// A node that serves no release: the reply is `None`, the machine is
/// untouched, and the executor is quiet again for the next interval.
#[tokio::test(flavor = "current_thread")]
async fn an_unpublished_network_leaves_the_machine_untouched() {
    let node = deployment();
    let client = fake_node(node.clone()).await;
    let rpc = client.origin().to_string();
    let updates = tempfile::tempdir().unwrap();
    let idle = Phase::Idle(app_update::Idle {
        current: Sha::digest(b"running"),
        previous: None,
        pinned_sequence: 0,
    });
    let mut updater = Updater::new(
        idle.clone(),
        keys(),
        updates.path().to_path_buf(),
        updates.path().join("state.json"),
    );
    assert_eq!(updater.tick(0, true), Some(Job::Fetch));
    let reply = run_job(rpc, keys(), updates.path().to_path_buf(), Job::Fetch).await;
    assert_eq!(reply, None);
    assert_eq!(updater.reply(reply), None);
    assert_eq!(updater.reading().phase, idle);
    assert_eq!(updater.reading().banner, None);
    assert_eq!(
        updater.tick(CHECK_INTERVAL_SECS, true),
        Some(Job::Fetch),
        "quiet again"
    );
}
