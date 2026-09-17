//! the crash-safety seam of the wasm files tenant: [`FilesOdbBacking`] driven on
//! a real tempdir. proves the four load-bearing properties the kernel relies on:
//!
//!   * **publish ordering on disk** — a staged object is durable BEFORE the refs
//!     commit point (the object side of the torn-commit fix), observed as the
//!     object file appearing while the refs file is still absent.
//!   * **query parity with native** — the guest query over the backing serves EXACTLY what a
//!     native `Files::query` serves on the identical committed dir (a metadata
//!     `Refs` query AND a body-reading `Read` query).
//!   * **snapshot / install round trip** — the refs image out, verify-then-adopt
//!     back in (the backing adopts the kernel-verified image).
//!   * **durable-height recovery** — a commit at height H stamps the refs
//!     envelope, and a reopen-after-drop recovers refs + H (the height the kernel
//!     threads through `publish_block`).

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use files::{
    Change, Content, FilesMsg, FilesQuery, Kind, Refs, encode_msg, encode_query, encode_refs,
    to_hex,
};
use files_odb::FilesOdbBacking;
use sdk::Module as _;
use sha2::{Digest as _, Sha256};
use wasm_host::{HostOdb as _, OdbBacking};

// ---- harness ----------------------------------------------------------------
// the three helpers this suite uses out of the duckfs test harness that stayed
// behind in `files/tests/harness`. inlined rather than shared: a `#[path]` into
// another crate's test tree would drag in the other sixty lines, all dead here.

/// a `files`-scoped ctx at block `height`, answering the identity query the
/// native `Files` makes while it builds the committed dir the backing opens on.
fn test_ctx(origin: sdk::Origin, height: u64) -> sdk_testkit::TestCtx {
    sdk_testkit::TestCtx::with_env(sdk::Env {
        height,
        consensus_time: height,
        origin,
        me: "files".into(),
        cause: sdk::Cause::Direct,
    })
    .on_query("identity", |req| {
        let identity::IdentityQuery::OfKey { .. } =
            identity::decode_query(req).map_err(sdk::Error::Module)?
        else {
            return Err(sdk::Error::QueryUnsupported);
        };
        Ok(identity::encode_reply(&identity::IdentityReply::Account(
            None,
        )))
    })
}

fn open_files(dir: &tempfile::TempDir) -> files::Files {
    files::Files::open("files", dir.path().to_path_buf()).expect("open")
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

// ---- helpers ----------------------------------------------------------------

/// commit one inline file through a native `Files` at `height`, persisting refs +
/// objects + the height-stamped envelope to `dir` — the committed state a backing
/// opened on the same dir must serve identically.
fn native_commit_inline(
    dir: &tempfile::TempDir,
    height: u64,
    path: &str,
    body: &[u8],
) -> files::Files {
    let mut f = open_files(dir);
    let op = sdk::Msg {
        target: "files".into(),
        payload: encode_msg(&FilesMsg::Commit {
            base_snapshot: None,
            message: "c".into(),
            changes: vec![Change::Put {
                path: path.into(),
                exec: false,
                meta: BTreeMap::new(),
                content: Content::Inline {
                    b64: STANDARD.encode(body),
                },
            }],
        }),
    };
    let mut ctx = test_ctx(sdk::Origin::System, height);
    futures::executor::block_on(f.execute(&mut ctx, &op)).expect("commit executes");
    futures::executor::block_on(f.commit_block()).expect("commit_block persists");
    f
}

fn open_backing(dir: &tempfile::TempDir) -> FilesOdbBacking {
    FilesOdbBacking::open("files", dir.path().to_path_buf()).expect("open backing")
}

// ============================================================================

/// the object side of the crash-safety ordering: `stage_put` only buffers,
/// `publish_block` makes the object durable, and the refs file does NOT exist
/// until `adopt_refs` — so a crash can never leave a refs image referencing an
/// object whose dir-entry never reached disk.
#[test]
fn publish_makes_objects_durable_before_the_refs_commit_point() {
    let d = tempfile::tempdir().unwrap();
    let mut backing = open_backing(&d);

    let body = b"a durable chunk";
    let id = backing.stage_put(Kind::Chunk.tag(), body);
    let hex = to_hex(&id);
    let obj_path = d.path().join("objects").join(&hex[..2]).join(&hex[2..]);
    let refs_path = d.path().join("refs");

    // stage_put only buffers — nothing on disk yet.
    assert!(!obj_path.exists(), "stage_put does not touch disk");
    assert!(!refs_path.exists(), "no refs file on a fresh dir");

    // publish: the object is durable; the refs commit point has NOT happened.
    backing.publish_block(9).expect("publish");
    assert!(
        obj_path.exists(),
        "publish_block flushed the staged object to disk"
    );
    assert!(
        !refs_path.exists(),
        "objects are durable BEFORE the refs file"
    );

    // adopt: only NOW does the refs file appear — the commit point, after objects.
    backing
        .adopt_refs(&encode_refs(&Refs::default()))
        .expect("adopt");
    assert!(refs_path.exists(), "adopt_refs is the refs commit point");
}

/// The deployed guest over the backing serves exactly what native `Files::query`
/// serves on the identical committed dir — a metadata `Refs` query and a
/// body-reading `Read` query (which reaches through to the on-disk object).
#[test]
fn query_matches_native_files_on_the_same_dir() {
    let d = tempfile::tempdir().unwrap();
    let native = native_commit_inline(&d, 2, "/a/hello.txt", b"hello odb");
    let backing = open_backing(&d);

    // root continuity: the backing's refs image is the native root preimage.
    assert_eq!(
        sha256(&backing.refs_bytes()),
        native.root().0,
        "root = sha256(refs_bytes) is byte-identical to native"
    );

    let guest = wasm_host::WasmModule::with_odb(
        "files",
        include_bytes!("../../../modules/apps/files/component.wasm"),
        Box::new(backing),
    )
    .expect("load files guest");
    let refs_q = encode_query(&FilesQuery::Refs {});
    let read_q = encode_query(&FilesQuery::Read {
        path: "/a/hello.txt".into(),
        snapshot: None,
        offset: 0,
        len: 9,
    });
    let stat_q = encode_query(&FilesQuery::Stat {
        path: "/a/hello.txt".into(),
        snapshot: None,
    });

    for (label, req) in [("refs", refs_q), ("read", read_q), ("stat", stat_q)] {
        let native_reply = futures::executor::block_on(native.query(&req)).expect("native query");
        let backing_reply = futures::executor::block_on(guest.query(&req)).expect("guest query");
        assert_eq!(
            backing_reply, native_reply,
            "backing {label} query must byte-match native",
        );
    }
}

/// snapshot out (the refs image), install back in (the kernel-verified image the
/// backing adopts). the destination backing's refs image round-trips the source
/// snapshot, and its root matches — the state-sync adopt seam.
#[test]
fn snapshot_out_adopt_in_round_trips_the_refs_image() {
    let src = tempfile::tempdir().unwrap();
    let _native = native_commit_inline(&src, 2, "/x.txt", b"snap me");
    let src_backing = open_backing(&src);
    let snapshot = src_backing.refs_bytes();
    assert_ne!(
        snapshot,
        encode_refs(&Refs::default()),
        "the snapshot has real state"
    );

    // install into a fresh backing (root verification is the kernel's job; the
    // backing adopts the verified image).
    let dst = tempfile::tempdir().unwrap();
    let mut dst_backing = open_backing(&dst);
    dst_backing
        .adopt_refs(&snapshot)
        .expect("adopt the snapshot");

    assert_eq!(
        dst_backing.refs_bytes(),
        snapshot,
        "install round-trips the refs image"
    );
    assert_eq!(
        sha256(&dst_backing.refs_bytes()),
        sha256(&src_backing.refs_bytes()),
        "the adopted root matches the source",
    );
}

/// the durable-height thread: the kernel captures the block height and hands it to
/// `publish_block`; the backing stamps it into the refs envelope at `adopt_refs`;
/// a reopen-after-drop recovers BOTH the committed refs and that height.
#[test]
fn reopen_recovers_refs_and_durable_height() {
    // a real committed refs image to adopt (built by a native Files).
    let src = tempfile::tempdir().unwrap();
    let native = native_commit_inline(&src, 3, "/a/hello.txt", b"recover me");
    let refs_image = native.snapshot();

    let d = tempfile::tempdir().unwrap();
    {
        let mut backing = open_backing(&d);
        assert_eq!(
            backing.durable_height(),
            0,
            "a fresh dir has no durable commit"
        );
        assert_eq!(
            backing.durable_commit_height(),
            None,
            "a fresh dir reports no cursor (native parity), NOT Some(0)",
        );

        // drive the backing's OWN commit sequence at height 42 (the kernel order:
        // publish captures the height, adopt stamps + saves it).
        backing.publish_block(42).expect("publish");
        backing.adopt_refs(&refs_image).expect("adopt");
        assert_eq!(backing.durable_height(), 42, "the committed height is live");
        assert_eq!(
            backing.refs_bytes(),
            refs_image,
            "the committed refs are live"
        );
    } // drop: only the durable refs envelope survives.

    let reopened = open_backing(&d);
    assert_eq!(
        reopened.durable_height(),
        42,
        "the durable height survived the reopen (envelope recovery)",
    );
    assert_eq!(
        reopened.durable_commit_height(),
        Some(42),
        "the recovery cursor survived the reopen — the trailing-block claim relies on it",
    );
    assert_eq!(
        reopened.refs_bytes(),
        refs_image,
        "the committed refs survived the reopen",
    );
}
