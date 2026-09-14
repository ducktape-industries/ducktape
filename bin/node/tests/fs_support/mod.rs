//! an in-process duckfs node for the CLI e2e, over noded's shared in-proc
//! daemon testkit: a REAL `host::Host::genesis` with files, identity and attribution,
//! fronted by noded's router on a loopback listener. the CLI subprocess
//! (`env!(CARGO_BIN_EXE_ducktape) fs`) drives it over http exactly as it would
//! a real daemon.
//!
//! the `NodeCommand` actor this used to hand-mirror now lives ONCE in
//! `noded::testkit`; this harness just builds the host and hands it over.
#![allow(dead_code)]

use std::process::Command;

use commonware_cryptography::{Signer as _, ed25519};
use host::Host;
use noded::testkit::InProcDaemon;

/// the password this harness's wallet is minted under. `ducktape fs commit`
/// SIGNS, so it opens that wallet and asks for this on stdin.
pub const WALLET_PASSWORD: &str = "harness-wallet";

/// a running in-process node plus the CLI-under-test's path.
pub struct Harness {
    // dropped BEFORE `dir` (fields drop in declaration order): the daemon's Drop
    // joins the actor, closing qmdb, so the tempdir is removed only afterward.
    daemon: InProcDaemon,
    dir: tempfile::TempDir,
}

impl Harness {
    /// stand up the node: genesis the files module on the testkit's actor thread
    /// and block until `/v1/status` answers.
    pub fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ducktape fs-e2e")
            .tempdir()
            .expect("harness tempdir");
        let duckfs_dir = dir.path().join("duckfs");
        let node_key = ed25519::PrivateKey::from_seed(rand::random()).public_key();
        let daemon = InProcDaemon::start_with_node_key(
            move || {
                let files = files::Files::open("files", duckfs_dir).expect("open files");
                Host::genesis(vec![
                    Box::new(identity::Identity::new(
                        "identity",
                        Box::new(sdk_testkit::MemStore::new()),
                        "fs-e2e".into(),
                    )),
                    Box::new(attribution::AttributionModule::new(
                        "attribution",
                        Box::new(sdk_testkit::MemStore::new()),
                    )),
                    Box::new(files),
                ])
                .expect("genesis")
            },
            vec!["files".into(), "identity".into(), "attribution".into()],
            node_key,
        );
        let harness = Harness { daemon, dir };
        harness.mint_wallet();
        harness
    }

    /// Mint the wallet `ducktape fs commit`/`pin` signs with.
    ///
    /// A duckfs write is charged to the key that signed the request — the
    /// commit's author, the `/home/<owner>/**` authority, the staging quota —
    /// so the verb has an identity or it has nothing to write as. A REAL
    /// `ducktape wallet new` rather than a hand-written key file: the format is
    /// the wallet verb's to define, and a fixture that hand-rolls it tests the
    /// fixture. A keystore is a WORKSPACE's, and this in-process node has
    /// none, so the wallet is minted into a bare one.
    fn mint_wallet(&self) {
        let out = Command::new(env!("CARGO_BIN_EXE_ducktape"))
            .args(["wallet", "new", "harness", "--workspace"])
            .arg(self.workspace())
            .stdin(self.password_pipe())
            .output()
            .expect("mint the harness wallet");
        assert!(
            out.status.success(),
            "the harness wallet must mint: {out:?}"
        );
    }

    /// the workspace the harness wallet lives in — a keystore and nothing
    /// else, since the node here runs in-process.
    fn workspace(&self) -> std::path::PathBuf {
        self.dir.path().join("workspace")
    }

    /// the harness wallet's key file, handed to every verb as
    /// `DUCKTAPE_USER_KEY`: a verb's own ladder finds a key through the
    /// workspace behind the node it dials, and this node has none.
    pub fn user_key(&self) -> std::path::PathBuf {
        keystore::wallet::key_file(&self.workspace(), "harness")
    }

    /// stdin for one CLI run: the wallet password, on a file rather than a pipe
    /// so `.output()` (which does not write stdin) still answers the prompt.
    fn password_pipe(&self) -> std::fs::File {
        let path = self.dir.path().join("wallet-password");
        if !path.exists() {
            std::fs::write(&path, format!("{WALLET_PASSWORD}\n")).expect("write the password");
        }
        std::fs::File::open(&path).expect("open the password")
    }

    /// the http base the CLI's `--node` flag takes.
    pub fn node_url(&self) -> String {
        self.daemon.node_url()
    }

    /// the header a mutating request carries — the harness owns this node, so
    /// it presents the operator credential the daemon minted.
    pub fn write_header(&self) -> (&'static str, &str) {
        self.daemon.write_header()
    }

    /// a duckfs transport whose writes this node admits: the harness owns the
    /// node, so it presents the operator credential the daemon minted.
    pub fn files(&self) -> duckfs_client::http::HttpNode {
        self.daemon.files()
    }

    /// a `ducktape fs` invocation pre-pointed at this node via `--node`.
    pub fn cli(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("fs")
            .args(args)
            .arg("--node")
            .arg(self.node_url())
            .env("DUCKTAPE_HOME", self.dir.path())
            .env("DUCKTAPE_USER_KEY", self.user_key())
            .stdin(self.password_pipe());
        cmd
    }

    /// a bare `ducktape fs` invocation (no `--node`) — for the resolution-error
    /// and stub-verb cases.
    ///
    /// `DUCKTAPE_HOME` points at this harness's own temp dir so the bottom rung
    /// of the addressing ladder (the lone workspace on the box) sees an EMPTY
    /// home — the harness workspace holds a keystore and no `node.toml`, so it
    /// is no network. Without it the run reads the developer's real
    /// `~/.ducktape` and the outcome depends on whose box it is.
    pub fn cli_bare(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("fs")
            .args(args)
            .env("DUCKTAPE_HOME", self.dir.path())
            .env("DUCKTAPE_USER_KEY", self.user_key())
            .stdin(self.password_pipe());
        cmd
    }
}
