//! The node, as this launcher sees it: a child process, and one verb it asks
//! questions of.
//!
//! The launcher holds no http client, no duckfs client and no consensus code.
//! Everything it needs to know about the chain it asks the `ducktape` binary
//! for — `ducktape release status --json` is the whole interface — and every
//! file it reads off the network it reads with `ducktape fs cat`. The node
//! serves the duckfs it downloads its successor from, and that is a file read
//! like any other.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use app_update::Designation;
use serde::Deserialize;

use crate::refusal::Refusal;

/// How long a SIGTERM'd node is given to checkpoint before it is killed. A
/// node writes its checkpoint on the way out; losing that turns a clean flip
/// into a journal replay on the way back up.
const STOP_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// What the running node answers `release status` with: where it serves, who
/// it is, where the chain is, and what the network has designated.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleaseStatus {
    /// The node's own http base — what `fs cat` dials.
    pub base: String,
    /// Empty until the node has published its mesh identity. A service daemon
    /// started before then exits fatal, so this is the supervisor's wait seam
    /// and the flipped release's healthy signal alike.
    #[serde(default)]
    pub public_key: String,
    #[serde(default)]
    pub height: u64,
    /// The release this network runs, and from which block. `None` until
    /// governance has passed one.
    #[serde(default)]
    pub designation: Option<Designation>,
}

impl ReleaseStatus {
    pub fn identity_published(&self) -> bool {
        !self.public_key.is_empty()
    }
}

/// A `ducktape` binary, pointed at one workspace.
#[derive(Debug, Clone)]
pub struct Ducktape {
    exe: PathBuf,
    config: PathBuf,
}

impl Ducktape {
    pub fn new(exe: PathBuf, config: PathBuf) -> Self {
        Ducktape { exe, config }
    }

    fn verb(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.exe);
        command
            .args(args)
            .arg("--config")
            .arg(&self.config)
            .stdin(Stdio::null());
        command
    }

    /// The running node's answer, or a refusal naming how the ask failed —
    /// which is also how "the node is not up yet" reads.
    pub fn status(&self) -> Result<ReleaseStatus, Refusal> {
        let output = self
            .verb(&["release", "status", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| Refusal::io("status_spawn_failed", &self.exe, &error))?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            return Err(Refusal::new(
                "status_unavailable",
                said.lines().next().unwrap_or("no output").to_string(),
            ));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| Refusal::new("status_unparsable", error.to_string()))
    }

    /// Read one duckfs file whole into `into`. There is no resume: `fs cat`
    /// streams a file from its start, and a read that fails is simply read
    /// again on the next poll.
    pub fn cat(&self, base: &str, duckfs_path: &str, into: &Path) -> Result<(), Refusal> {
        if let Some(parent) = into.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Refusal::io("download_failed", parent, &error))?;
        }
        let file = std::fs::File::create(into)
            .map_err(|error| Refusal::io("download_failed", into, &error))?;
        let status = Command::new(&self.exe)
            .args(["fs", "cat", duckfs_path, "--node", base])
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| Refusal::io("download_failed", &self.exe, &error))?;
        if status.status.success() {
            return Ok(());
        }
        let _ = std::fs::remove_file(into);
        let said = String::from_utf8_lossy(&status.stderr);
        Err(Refusal::new(
            "download_failed",
            format!(
                "fs cat {duckfs_path}: {}",
                said.lines().next().unwrap_or("no output")
            ),
        ))
    }

    /// Ask a STAGED binary whether it can run this workspace: it reopens the
    /// checkpoint and recomposes the committed root hash, which is exactly the
    /// restart path a live node takes. Its first stdout line is the reason
    /// when it cannot.
    pub fn qualify(staged: &Path, config: &Path) -> Result<(), Unqualified> {
        let output = Command::new(staged)
            .args(["node", "qualify"])
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|error| {
                Unqualified::from(Refusal::io("qualify_spawn_failed", staged, &error))
            })?;
        if output.status.success() {
            return Ok(());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let printed = stdout.lines().next().map(str::trim).unwrap_or_default();
        let ended = match output.status.code() {
            Some(code) => format!("exited {code}"),
            None => "killed".to_string(),
        };
        // The binary's stderr already reached this launcher's log; its token
        // is the part only it can name.
        let gave_a_token = is_token(printed);
        match gave_a_token {
            true => Err(Unqualified {
                reason: printed.to_string(),
                detail: ended,
            }),
            false => Err(Unqualified {
                reason: "qualify_refused".to_string(),
                detail: format!("{ended}; printed {printed:?}"),
            }),
        }
    }

    /// Start the child this launcher supervises: the install path's binary,
    /// with `args` after the workspace selector.
    pub fn spawn(&self, args: &[OsString]) -> Result<Child, Refusal> {
        let inner = Command::new(&self.exe)
            .args(args)
            .arg("--config")
            .arg(&self.config)
            .spawn()
            .map_err(|error| Refusal::io("spawn_failed", &self.exe, &error))?;
        Ok(Child { inner })
    }
}

/// Why a staged binary will not run this workspace. `reason` is the binary's
/// OWN snake_case token — `node qualify` prints one on its first stdout line,
/// by contract — or this launcher's `qualify_refused` when it gave none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unqualified {
    pub reason: String,
    pub detail: String,
}

impl From<Refusal> for Unqualified {
    fn from(refusal: Refusal) -> Self {
        Unqualified {
            reason: refusal.reason.to_string(),
            detail: refusal.detail,
        }
    }
}

/// A `reason` field is a stable snake_case token, never prose: what a staged
/// binary prints is trusted as one only when it is one.
fn is_token(line: &str) -> bool {
    let token_bytes = line
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    !line.is_empty() && token_bytes
}

/// The supervised process.
#[derive(Debug)]
pub struct Child {
    inner: std::process::Child,
}

impl Child {
    /// `Some(code)` once it is gone; `None` while it runs.
    pub fn exited(&mut self) -> Option<Option<i32>> {
        match self.inner.try_wait() {
            Ok(Some(status)) => Some(status.code()),
            // A child we cannot wait on is a child we no longer own.
            Err(_) => Some(None),
            Ok(None) => None,
        }
    }

    /// SIGTERM, then the checkpoint budget, then SIGKILL. Never the other way
    /// round: a node killed mid-apply comes back through a journal replay.
    pub fn stop(&mut self) {
        let pid = self.inner.id() as libc::pid_t;
        // SAFETY: `pid` is this launcher's own child, which has not been
        // reaped — `try_wait`/`wait` are the only reapers and both are here.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + STOP_BUDGET;
        while std::time::Instant::now() < deadline {
            if self.exited().is_some() {
                return;
            }
            std::thread::sleep(STOP_POLL);
        }
        tracing::warn!(
            target: crate::TARGET,
            event = "node_update_refused",
            reason = "stop_timed_out",
            pid,
            "the node did not checkpoint within the stop budget; killing it"
        );
        let _ = self.inner.kill();
        let _ = self.inner.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_answer_decodes_with_and_without_a_designation() {
        let bare: ReleaseStatus = serde_json::from_str(
            r#"{"base":"http://127.0.0.1:8844","public_key":"","height":0,"designation":null}"#,
        )
        .unwrap();
        assert!(!bare.identity_published());
        assert_eq!(bare.designation, None);

        let sha = app_update::Sha::digest(b"node");
        let designated: ReleaseStatus = serde_json::from_str(&format!(
            r#"{{"base":"http://127.0.0.1:8844","public_key":"ab12","height":900,
                 "designation":{{"sha256":"{sha}","activation_height":1200}}}}"#
        ))
        .unwrap();
        assert!(designated.identity_published());
        let designation = designated.designation.expect("a designation");
        assert_eq!(designation.sha256, sha);
        assert!(!designation.armed_at(designated.height));
        assert!(designation.armed_at(1200));
    }
}
