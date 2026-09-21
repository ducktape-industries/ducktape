//! the sandbox backend seam: how a provider child is spawned. every backend
//! is an audited in-tree adapter — a run NEVER executes bare on the host, so
//! the seam has no unsandboxed variant a config could select.
//!
//! This module owns the [`SandboxBackend`] enum + its boot probe. The backend's
//! execution — the VM configuration ([`crate::firecracker_api`]), the run
//! lifecycle ([`crate::microvm`]) and the block images
//! ([`crate::workspace_image`]) — lives beside it.

use std::path::{Path, PathBuf};

/// which VMM boots a run's microVM. The run lifecycle — images, vsock frames,
/// manifest, teardown — is identical either way; the flavor decides only the
/// hypervisor binary, its boot arguments and what the host probe must verify.
/// One flavor per OS: Firecracker needs KVM, the vz shim needs
/// Virtualization.framework.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vmm {
    /// `firecracker` on Linux, over `/dev/kvm`.
    Firecracker,
    /// `duck-vz-shim` on macOS (`bin/duck-vz-shim`): a thin Swift wrapper over
    /// Virtualization.framework that consumes the same Firecracker-schema
    /// config JSON and bridges guest vsock ports to the same `<uds>_<port>`
    /// unix sockets, so the host side of a run is byte-identical.
    Vz,
}

impl Vmm {
    /// the host VMM binary this flavor execs.
    pub fn host_bin(&self) -> &'static str {
        match self {
            Vmm::Firecracker => "firecracker",
            Vmm::Vz => "duck-vz-shim",
        }
    }

    /// the `[sandbox] runtime` token that names this flavor in node.toml —
    /// shared by config resolution and the `node init` table writer so the two
    /// can never drift.
    pub fn config_token(&self) -> &'static str {
        match self {
            Vmm::Firecracker => "firecracker",
            Vmm::Vz => "vz",
        }
    }

    /// the flavor this OS boots — the ONE place the OS→hypervisor decision
    /// lives, so `node init`, the e2e harness and the smoke example cannot
    /// drift apart.
    pub fn platform_default() -> Self {
        if cfg!(target_os = "macos") {
            Vmm::Vz
        } else {
            Vmm::Firecracker
        }
    }
}

/// how a provider child is spawned — always inside an isolation adapter; a
/// bare host spawn is unrepresentable here by design ("nothing ever runs
/// directly on the node"). `MicroVm` gives each run its own microVM. a node
/// sandboxes EVERY run it makes — demandless ones included — because a
/// sandboxed node sandboxes everything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    /// one microVM per run: hard vcpu/memory limits enforced by the
    /// hypervisor, no container runtime inside the guest. `kernel` and `rootfs`
    /// are the immutable images every run boots from — shared read-only, never
    /// written by a run. `vmm` picks the hypervisor for this OS.
    MicroVm {
        vmm: Vmm,
        kernel: std::path::PathBuf,
        rootfs: std::path::PathBuf,
        /// the operator's agent CLIs, filled by `ducktape agent install`. This
        /// directory is BOTH what the node announces from and what
        /// [`crate::executor_image`] derives the guest's copy from, so a node
        /// can no longer advertise a capability its guest cannot exec.
        executors: std::path::PathBuf,
    },
    /// test-harness spawn: the bin exec'd directly, compiled ONLY into test
    /// builds so the run loop stays testable without a container runtime on the
    /// test host. a shipped binary cannot express a bare spawn — the variant
    /// does not exist outside `cfg(test)` / the `testkit` feature, and nothing
    /// but a dev-dependency turns that feature on.
    #[cfg(any(test, feature = "testkit"))]
    Bare,
}

impl SandboxBackend {
    /// the host runtime binary this adapter drives.
    pub fn runtime_bin(&self) -> &'static str {
        match self {
            SandboxBackend::MicroVm { vmm, .. } => vmm.host_bin(),
            #[cfg(any(test, feature = "testkit"))]
            SandboxBackend::Bare => "sh",
        }
    }

    /// the host tools this adapter shells out to, each paired with what it is
    /// for. Checked at boot alongside [`Self::runtime_bin`] so a host missing
    /// one fails loudly here instead of mid-run.
    ///
    /// The pairing is load-bearing for the error message. A tool once went
    /// missing from this list under the previous backend; a host without it
    /// passed this probe, ran the e2e suite, and failed 156 s later as `timed
    /// out waiting for the agent reply to post` — a message naming neither the
    /// runtime nor the tool, which reads like a product defect. A guard that
    /// reports "ready" while the runtime cannot start a run is worse than no
    /// guard.
    fn required_tools(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            // both VMMs give the guest no tap device — its whole reach is
            // vsock — so there is no interface for an egress firewall to sit
            // on and nothing here shells out to `nft`.
            SandboxBackend::MicroVm { .. } => &[
                ("mke2fs", "builds each run's workspace block image"),
                ("debugfs", "reads that image back after the guest exits"),
            ],
            #[cfg(any(test, feature = "testkit"))]
            SandboxBackend::Bare => &[],
        }
    }

    /// whether this run spawns through the test-only bare harness (host paths,
    /// no mount canonicalization). always false in shipped code.
    pub fn is_bare_test(&self) -> bool {
        #[cfg(any(test, feature = "testkit"))]
        {
            matches!(self, SandboxBackend::Bare)
        }
        #[cfg(not(any(test, feature = "testkit")))]
        {
            false
        }
    }

    /// verify this host can actually run the chosen adapter: the runtime binary
    /// must be executable somewhere on `PATH`, every tool in
    /// [`Self::required_tools`] must be present, and the adapter's own host
    /// capabilities must hold. a config naming an unusable runtime is a loud
    /// boot error — there is no bare fallback.
    ///
    /// All of it is a hard dependency, so a missing one fails at boot, never as
    /// a silently unsandboxed / unfirewalled run.
    pub fn probe(&self) -> Result<PathBuf, String> {
        let found = self.probe_adapter()?;
        self.probe_images()?;
        // Once per DAEMON BOOT — one of the crate's two `info` slots, and the only
        // line that separates "this daemon probed green" from "this daemon
        // never reached the probe". The FAILURE arms stay as the returned Err:
        // the compute daemon dies on it and the operator reads it from main, so
        // an `error!` here would double-report.
        tracing::info!(
            target: "ducktape::sandbox",
            event = "sandbox_backend_ready",
            backend = self.runtime_bin(),
            "the sandbox backend probed green"
        );
        Ok(found)
    }

    /// Everything about the HOST: the runtime binary, the tools it shells out
    /// to, and the adapter-specific capability a tool check cannot express
    /// (`/dev/kvm` opens; the vz shim carries its entitlement). Returns the
    /// resolved VMM binary.
    ///
    /// Split out of [`Self::probe`] because "can this host isolate a run" and
    /// "are this node's images built" are asked at different times: `node
    /// init`/`join` decide whether to write a live `[sandbox]` table BEFORE
    /// `ops/build-guest-rootfs.sh` has ever run.
    pub fn probe_adapter(&self) -> Result<PathBuf, String> {
        let bin = self.runtime_bin();
        let found = crate::host_tools::find_executable(bin).ok_or_else(|| {
            format!(
                "sandbox runtime {bin:?} is not executable on PATH or a standard bin dir; \
                 install it or pick a runtime this host provides"
            )
        })?;
        for (tool, why) in self.required_tools() {
            if crate::host_tools::find_system_tool(tool).is_none() {
                return Err(format!(
                    "{tool} is not executable on PATH or a standard sbin dir; the {bin} sandbox \
                     requires it ({why}) — install it"
                ));
            }
        }
        self.probe_host_capabilities(&found)?;
        Ok(found)
    }

    /// the guest artifacts this node's `[sandbox]` table points at. A boot
    /// concern only: the operator standing here already chose the table.
    fn probe_images(&self) -> Result<(), String> {
        match self {
            SandboxBackend::MicroVm { kernel, rootfs, .. } => {
                for (label, image) in [("kernel", kernel), ("rootfs", rootfs)] {
                    if !image.is_file() {
                        return Err(format!(
                            "the microVM {label} image is missing or not a file; build it with \
                             ops/build-guest-rootfs.sh, or point [sandbox] at one that exists"
                        ));
                    }
                }
                Ok(())
            }
            #[cfg(any(test, feature = "testkit"))]
            SandboxBackend::Bare => Ok(()),
        }
    }

    /// the adapter-specific host state a tool check cannot express. `runtime`
    /// is the resolved VMM binary, which the vz probe inspects for its
    /// entitlement.
    fn probe_host_capabilities(&self, runtime: &Path) -> Result<(), String> {
        match self {
            SandboxBackend::MicroVm { vmm, .. } => match vmm {
                Vmm::Firecracker => probe_kvm(),
                Vmm::Vz => probe_vz(runtime),
            },
            #[cfg(any(test, feature = "testkit"))]
            SandboxBackend::Bare => Ok(()),
        }
    }
}

/// `/dev/kvm` must open read-write for THIS process — the check that actually
/// predicts a boot.
///
/// Presence is not enough and neither is group membership on paper: `usermod -aG
/// kvm` does not touch a session that is already running, so a host can list the
/// group and still get `EACCES` until the next login. Opening the device is the
/// only question whose answer matches what Firecracker will see.
fn probe_kvm() -> Result<(), String> {
    let dev = Path::new("/dev/kvm");
    if !dev.exists() {
        return Err(
            "/dev/kvm is absent; the Firecracker sandbox needs hardware virtualisation — check \
             that the CPU exposes vmx/svm and that the kvm module is loaded"
                .into(),
        );
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev)
        .map(drop)
        .map_err(|e| {
            format!(
                "/dev/kvm exists but this process cannot open it read-write ({e}); add the node's \
                 user to the kvm group (a running session does not pick that up — log in again, \
                 or start the node under `sg kvm`)"
            )
        })
}

/// the vz backend's host state: Hypervisor.framework support, and the shim
/// binary carrying the virtualization entitlement.
///
/// The entitlement is checked HERE because a shim without it fails at
/// `VZVirtualMachine` creation — a per-run error deep in the boot path that
/// names Virtualization.framework internals — while the fix (re-run the shim's
/// build script, which codesigns) belongs at daemon boot, in front of an
/// operator.
#[cfg(target_os = "macos")]
fn probe_vz(shim: &Path) -> Result<(), String> {
    let hv = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.hv_support"])
        .output()
        .map_err(|e| format!("run sysctl kern.hv_support: {e}"))?;
    let supported = String::from_utf8_lossy(&hv.stdout).trim() == "1";
    if !supported {
        return Err(
            "this Mac reports no Hypervisor.framework support (kern.hv_support != 1); \
             the vz sandbox needs Apple silicon or a VT-x Mac"
                .into(),
        );
    }
    let entitlements = std::process::Command::new("/usr/bin/codesign")
        .args(["--display", "--entitlements", "-", "--xml"])
        .arg(shim)
        .output()
        .map_err(|e| format!("run codesign on {}: {e}", shim.display()))?;
    let mut signed = entitlements.stdout;
    signed.extend_from_slice(&entitlements.stderr);
    let entitled = String::from_utf8_lossy(&signed).contains("com.apple.security.virtualization");
    if !entitled {
        return Err(format!(
            "{} is not signed with the com.apple.security.virtualization entitlement, so \
             Virtualization.framework will refuse it; rebuild it with bin/duck-vz-shim/build.sh \
             (which codesigns)",
            shim.display()
        ));
    }
    Ok(())
}

/// the vz backend exists only where Virtualization.framework does.
#[cfg(not(target_os = "macos"))]
fn probe_vz(_shim: &Path) -> Result<(), String> {
    Err(
        "the vz sandbox runs only on macOS (it drives Virtualization.framework); \
         on Linux use runtime = \"firecracker\""
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The image check is the BOOT probe's, not the adapter's: `node
    /// init`/`join` decide whether to write a live `[sandbox]` table before
    /// `ops/build-guest-rootfs.sh` has ever run, so folding this back into
    /// [`SandboxBackend::probe_adapter`] makes a machine that can isolate runs
    /// write the commented-out table instead.
    #[test]
    fn missing_images_fail_the_boot_probe_and_name_the_builder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = SandboxBackend::MicroVm {
            vmm: Vmm::platform_default(),
            kernel: dir.path().join("vmlinux"),
            rootfs: dir.path().join("rootfs.ext4"),
            executors: dir.path().join("executors"),
        };
        let error = backend
            .probe_images()
            .expect_err("absent images must refuse");
        assert!(error.contains("kernel"), "names which image: {error}");
        assert!(
            error.contains("ops/build-guest-rootfs.sh"),
            "names the builder: {error}"
        );

        std::fs::write(dir.path().join("vmlinux"), b"k").expect("kernel");
        std::fs::write(dir.path().join("rootfs.ext4"), b"r").expect("rootfs");
        backend.probe_images().expect("present images pass");
    }
}
